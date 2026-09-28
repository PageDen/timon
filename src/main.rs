use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use std::ffi::OsString;
use std::io::Read;
use std::num::NonZeroU16;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use timon::admission::{Ledger, RunLimits};
use timon::attempt::{AttemptSpec, Role, UsageSource, run_attempt};
use timon::bridge;
use timon::broker;
use timon::mcp::tools::WorkerPolicy;
use timon::recorder::client::{send as recorder_send, send_line as recorder_send_line};
use timon::recorder::db::{RESTORE_CAVEAT, Store, plan_restore, restore};
use timon::recorder::event::{MAX_REQUEST_BYTES, UsageEvent};
use timon::recorder::producer::{
    DEFAULT_MAX_SPOOLED_EVENTS, Delivery, Spool, deliver, event_for, replay,
};
use timon::recorder::protocol::{Request, Response};
use timon::recorder::render;
use timon::recorder::retain::{self, DEFAULT_KEEP_DAYS};
use timon::recorder::server::{Config as RecorderConfig, serve as serve_recorder};
use timon::research::verify::{self, Findings};
use timon::usage::Accumulation;
use timon::worker::slots::{SlotError, SlotMode, SlotPool};
use timon::worker::{WorkerLimits, WorkerSpec};

/// Exit code when no worker slot is free (EX_TEMPFAIL).
const EXIT_NO_SLOT: u8 = 75;
/// The run's own allowance is used up (EX_UNAVAILABLE).
///
/// Distinct from `EXIT_NO_SLOT`: a busy host frees up, a spent run does not, so
/// a caller should not retry this one.
const EXIT_RUN_ALLOWANCE: u8 = 69;
const EXIT_ATTEMPT_FAILED: u8 = 1;
/// The process succeeded but its typed result is missing or invalid (EX_DATAERR).
const EXIT_RESULT_UNUSABLE: u8 = 65;
const EXIT_TIMED_OUT: u8 = 124;
const EXIT_CANCELLED: u8 = 130;

#[derive(Parser)]
#[command(name = "timon", version, about = "Lead model steering bounded workers")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// The strong model that plans, integrates and verifies.
    #[command(subcommand)]
    Lead(AttemptCommand),
    /// A cheaper model running one bounded task.
    #[command(subcommand)]
    Worker(AttemptCommand),
    /// Record reported token usage per account (amendment A1).
    #[command(subcommand)]
    Usage(UsageCommand),
    /// Make a Codex Desktop or IDE session visible to the usage recorder
    Bridge(BridgeArgs),
    /// Pooled provider credentials for quota rotation (amendment A4)
    #[command(subcommand)]
    Broker(BrokerCommand),
    /// Operator commands for the host-wide worker slots.
    #[command(subcommand)]
    Slots(SlotsCommand),
    /// Check whether cited claims are supported by the pages they cite.
    #[command(subcommand)]
    Research(ResearchCommand),
    /// Serve the delegation tool over stdio, for a lead's harness to launch.
    Mcp(McpArgs),
    /// Run a goal through plan, delegate and integrate.
    Orchestrate(OrchestrateArgs),
    /// Hand a goal to Timon: preflight first, then a run that outlives the terminal.
    Run(HandoffArgs),
    /// Serve the hand-off tool over stdio, for a developer's Codex session to call.
    Handoff(HandoffServeArgs),
    /// Inspect and cancel hand-offs.
    #[command(subcommand)]
    Runs(RunsCommand),
    /// Show how a goal would be routed, without recording or running anything.
    Triage(TriageArgs),
}

/// Where the run store lives by default.
///
/// Under the caller's own home, because a run belongs to the person who started
/// it and a shared file would make one developer's hand-offs visible to another.
fn default_run_store() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".timon").join("runs.sqlite")
}

#[derive(Args)]
struct HandoffArgs {
    /// What the run should achieve.
    goal: String,
    /// The repository the work is rooted in. Defaults to the current directory.
    #[arg(long)]
    workspace: Option<PathBuf>,
    /// Pooled accounts this run may spend. Repeatable. Omit to let the broker choose.
    #[arg(long = "account")]
    accounts: Vec<String>,
    /// Attempts this run may start.
    #[arg(long, default_value_t = 64)]
    max_attempts: u32,
    /// Token admission ceiling. An estimate that decides whether to start more
    /// work, never a cap on what running work spends.
    #[arg(long)]
    token_ceiling: Option<u64>,
    /// Seconds from now after which no further work is started.
    #[arg(long)]
    deadline_secs: Option<i64>,
    /// Makes resubmission idempotent: the same key returns the same run.
    #[arg(long)]
    submission_key: Option<String>,
    /// Insist on a route instead of letting triage choose: cheap, strong or planner.
    #[arg(long, value_parser = ["cheap", "strong", "planner"])]
    route: Option<String>,
    /// Allow the planner path. Without this the most that happens is one strong call.
    #[arg(long)]
    allow_planner: bool,
    /// The run store.
    #[arg(long)]
    store: Option<PathBuf>,
    /// Record the run and report it, without starting the pipeline. The pipeline
    /// itself is P2 onwards; this is what P1 delivers.
    #[arg(long, default_value_t = true)]
    preflight_only: bool,
    #[arg(long, value_enum, default_value_t = ReportFormat::Text)]
    format: ReportFormat,
}

#[derive(Args)]
struct TriageArgs {
    /// The goal to route.
    #[arg(long)]
    goal: String,
    /// Allow the planner path.
    #[arg(long)]
    allow_planner: bool,
    /// Insist on a route, to see it recorded as the caller's choice.
    #[arg(long, value_parser = ["cheap", "strong", "planner"])]
    route: Option<String>,
    #[arg(long, value_enum, default_value_t = ReportFormat::Text)]
    format: ReportFormat,
}

#[derive(Args)]
struct HandoffServeArgs {
    /// The run store. Defaults to the caller's own.
    #[arg(long)]
    store: Option<PathBuf>,
    /// The repository this server serves. Authoritative: a session may narrow to
    /// a path inside it, not redirect outside it. Defaults to the directory the
    /// server was started in.
    #[arg(long)]
    workspace: Option<PathBuf>,
    /// Attempts a hand-off gets when it does not ask.
    #[arg(long, default_value_t = 64)]
    default_max_attempts: u32,
    /// The most attempts a hand-off may ask for. A session cannot raise this.
    #[arg(long, default_value_t = 256)]
    max_attempts_limit: u32,
}

#[derive(Subcommand)]
enum RunsCommand {
    /// Show recent hand-offs and their state
    List(RunsListArgs),
    /// Show one hand-off
    Show(RunsShowArgs),
    /// Ask a hand-off to stop
    Cancel(RunsCancelArgs),
    /// Mark runs left behind by a stopped orchestrator, and report them
    Recover(RunsRecoverArgs),
}

#[derive(Args)]
struct RunsListArgs {
    #[arg(long)]
    store: Option<PathBuf>,
    #[arg(long, default_value_t = 20)]
    limit: usize,
    #[arg(long, value_enum, default_value_t = ReportFormat::Text)]
    format: ReportFormat,
}

#[derive(Args)]
struct RunsShowArgs {
    id: String,
    #[arg(long)]
    store: Option<PathBuf>,
    #[arg(long, value_enum, default_value_t = ReportFormat::Text)]
    format: ReportFormat,
}

#[derive(Args)]
struct RunsCancelArgs {
    id: String,
    #[arg(long)]
    store: Option<PathBuf>,
}

#[derive(Args)]
struct RunsRecoverArgs {
    #[arg(long)]
    store: Option<PathBuf>,
    #[arg(long, value_enum, default_value_t = ReportFormat::Text)]
    format: ReportFormat,
}

#[derive(Args)]
struct OrchestrateArgs {
    #[arg(long)]
    run_id: String,
    /// The goal, or `-` for stdin.
    #[arg(long)]
    goal: String,
    #[arg(long)]
    output_root: PathBuf,
    /// Most tasks the lead may ask for.
    #[arg(long, default_value_t = 4)]
    max_tasks: usize,
    #[arg(long, default_value_t = 300)]
    lead_deadline_secs: u64,
    #[arg(long, default_value_t = 300)]
    worker_deadline_secs: u64,
    #[arg(long, default_value_t = timon::worker::DEFAULT_MAX_TASK_BYTES)]
    max_task_bytes: usize,
    #[arg(long, default_value_t = timon::worker::DEFAULT_MAX_OUTPUT_BYTES)]
    max_output_bytes: u64,
    /// Schema the workers' and the final answer are held to.
    #[arg(long)]
    deliverable_schema: Option<PathBuf>,
    #[arg(long, requires = "slots")]
    slot_dir: Option<PathBuf>,
    #[arg(long, requires = "slot_dir")]
    slots: Option<NonZeroU16>,
    #[arg(long, requires = "slot_dir")]
    provisioned_slots: bool,
    #[arg(long)]
    run_ledger: Option<PathBuf>,
    #[arg(long, default_value_t = 64)]
    run_max_attempts: u32,
    #[arg(long)]
    run_token_ceiling: Option<u64>,
    #[arg(long, default_value_t = timon::admission::DEFAULT_ATTEMPT_RESERVE)]
    attempt_reserve: u64,
    #[arg(long, default_value = "stdout")]
    usage_source: String,
    #[arg(long, value_enum, default_value_t = UsageAccounting::Delta)]
    usage_accounting: UsageAccounting,
    /// The lead command, as a JSON array of arguments.
    ///
    /// A JSON array rather than a bare argument list because two commands that
    /// each contain their own flags cannot be told apart on one command line,
    /// and splitting a string here would mean guessing at quoting. Placeholders
    /// {result}, {schema} and {attempt_dir} are filled in per phase, and the
    /// prompt arrives on stdin.
    ///
    /// Example: --lead-command '["codex","exec","-s","read-only","-o","{result}","-"]'
    #[arg(long, required = true)]
    lead_command: String,
    /// The worker command, same form and same placeholders.
    #[arg(long, required = true)]
    worker_command: String,
}

#[derive(Args)]
struct McpArgs {
    /// Identifies the run every delegated worker is accounted to.
    #[arg(long)]
    run_id: String,
    /// Directory the workers' attempt directories are made under.
    #[arg(long)]
    output_root: PathBuf,
    /// Seconds a delegated worker may run before it is stopped.
    #[arg(long, default_value_t = 300)]
    worker_deadline_secs: u64,
    #[arg(long, default_value_t = timon::worker::DEFAULT_MAX_TASK_BYTES)]
    max_task_bytes: usize,
    #[arg(long, default_value_t = timon::worker::DEFAULT_MAX_OUTPUT_BYTES)]
    max_output_bytes: u64,
    /// Host-wide slot directory, so delegated workers share the host limit.
    #[arg(long, requires = "slots")]
    slot_dir: Option<PathBuf>,
    #[arg(long, requires = "slot_dir")]
    slots: Option<NonZeroU16>,
    #[arg(long, requires = "slot_dir")]
    provisioned_slots: bool,
    /// Admission ledger, so delegation is bounded by the run's allowance.
    #[arg(long)]
    run_ledger: Option<PathBuf>,
    #[arg(long, default_value_t = 64)]
    run_max_attempts: u32,
    #[arg(long)]
    run_token_ceiling: Option<u64>,
    #[arg(long, default_value_t = timon::admission::DEFAULT_ATTEMPT_RESERVE)]
    attempt_reserve: u64,
    /// Where a worker's usage events are read from.
    #[arg(long, default_value = "stdout")]
    usage_source: String,
    #[arg(long, value_enum, default_value_t = UsageAccounting::Delta)]
    usage_accounting: UsageAccounting,
    /// File a worker is told to write its result to, inside its own attempt
    /// directory, and the schema it is held to.
    #[arg(long)]
    result_file: Option<String>,
    #[arg(long, requires = "result_file")]
    result_schema: Option<PathBuf>,
    /// The worker command. The task is delivered on stdin, never as an argument.
    #[arg(last = true, required = true, num_args = 1..)]
    command: Vec<OsString>,
}

#[derive(Subcommand)]
enum ResearchCommand {
    /// Verify a findings document.
    Verify(VerifyArgs),
}

#[derive(Args)]
struct VerifyArgs {
    /// Findings to check, or `-` for stdin.
    #[arg(long)]
    findings: String,
    /// Whole-check deadline per claim.
    #[arg(long, default_value_t = 20)]
    timeout_secs: u64,
    #[arg(long, value_enum, default_value_t = VerifyFormat::Text)]
    format: VerifyFormat,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum VerifyFormat {
    Text,
    Json,
}

#[derive(Subcommand)]
enum SlotsCommand {
    /// Create the shared slot directory and lock files. Run as root, once per
    /// host; every official launcher then shares this one limit.
    Provision(ProvisionArgs),
}

#[derive(Args)]
struct ProvisionArgs {
    #[arg(long)]
    dir: PathBuf,
    /// How many workers may run on this host at once, across all accounts.
    #[arg(long)]
    slots: NonZeroU16,
    /// Group allowed to lock the slots. Members can open and lock them; the
    /// directory stays un-writable by the group so they cannot unlink, replace
    /// or add one.
    #[arg(long)]
    group: Option<String>,
    #[arg(long, default_value = "750")]
    dir_mode: String,
    #[arg(long, default_value = "660")]
    file_mode: String,
}

#[derive(Subcommand)]
enum UsageCommand {
    /// Run the recorder daemon.
    Daemon(DaemonArgs),
    /// Send one usage event, read as JSON from stdin.
    Append(AppendArgs),
    /// Read recorded usage.
    Query(QueryArgs),
    /// Total recorded usage over a window, grouped by account.
    Report(ReportArgs),
    /// Deliver events left in this account's spool by an earlier outage.
    Replay(ReplayArgs),
    /// Take a verified snapshot of the database.
    Backup(BackupArgs),
    /// Put a verified snapshot back in place.
    Restore(RestoreArgs),
    /// Read rolled-up monthly totals, which outlive the detail behind them.
    Monthly(MonthlyArgs),
    /// Roll detail older than the retention window into monthly totals.
    Retain(RetainArgs),
    /// Record that an account was removed, so the next holder of its uid starts
    /// a new generation instead of inheriting its history.
    RetireUid(RetireUidArgs),
}

#[derive(Subcommand)]
enum BrokerCommand {
    /// Show the pooled accounts, and whether rotation is possible
    Accounts(BrokerAccountsArgs),
    /// Forward Codex traffic, injecting a pooled credential the caller cannot see
    Serve(BrokerServeArgs),
    /// Renew an account's access token now, without waiting for it to near expiry
    Refresh(BrokerRefreshArgs),
}

#[derive(Args)]
struct BrokerRefreshArgs {
    /// The account store.
    #[arg(long, default_value = "/var/lib/timon-broker/accounts")]
    store: PathBuf,
    /// Which account to renew. Omit to renew every account that needs it.
    #[arg(long)]
    account: Option<String>,
    /// Renew even when the current token is not near expiry.
    ///
    /// The case this exists for: a token can be *invalidated* by the provider
    /// while still unexpired — changing a subscription plan does it — and the
    /// clock cannot see that.
    #[arg(long)]
    force: bool,
}

#[derive(Args)]
struct BrokerServeArgs {
    /// The account store.
    #[arg(long, default_value = "/var/lib/timon-broker/accounts")]
    store: PathBuf,
    /// Serve only this pooled account. Omit to serve the whole pool, which is
    /// the normal case: the broker then chooses per request.
    #[arg(long)]
    account: Option<String>,
    /// Loopback address to listen on. Refuses to bind anywhere else: this holds
    /// credentials for everyone, and reaching it must require being on the host.
    #[arg(long, default_value = "127.0.0.1:1456")]
    listen: String,
    /// Where to forward. Defaults to the endpoint Codex uses itself.
    #[arg(long)]
    upstream: Option<String>,
    /// Seconds a client may hold a connection without completing a request.
    #[arg(long, default_value_t = 120)]
    read_timeout_secs: u64,
    /// The model interactive sessions get, whatever they ask for.
    ///
    /// Omit to leave requests untouched, which is the default: installing this
    /// release changes nothing until somebody decides it should.
    #[arg(long)]
    assign_model: Option<String>,
    /// A model a caller may keep if it asks for it. Repeatable.
    ///
    /// For when a developer legitimately needs a specific model and the operator
    /// agreed in advance.
    #[arg(long = "allow-model")]
    allow_models: Vec<String>,
    /// Names the policy, so a substitution can say which rule produced it.
    #[arg(long, default_value = "default")]
    model_policy_version: String,
}

#[derive(Args)]
struct BrokerAccountsArgs {
    /// The account store. One subdirectory per pooled account.
    #[arg(long, default_value = "/var/lib/timon-broker/accounts")]
    store: PathBuf,
    #[arg(long, value_enum, default_value_t = ReportFormat::Text)]
    format: ReportFormat,
}

#[derive(Args)]
struct BridgeArgs {
    /// The recorder socket. Omit to proxy without recording, which is how the
    /// pass-through is checked without a daemon running.
    #[arg(long)]
    socket: Option<PathBuf>,
    /// This account's spool. Defaults to the one an attempt would use, so a
    /// recorder outage costs a delay rather than the record.
    #[arg(long)]
    spool: Option<PathBuf>,
    /// Model to attribute. The protocol's usage notification does not name one.
    #[arg(long)]
    model: Option<String>,
    /// Report what was proxied and recorded, as JSON, when the child exits.
    #[arg(long)]
    report: bool,
    /// The app-server to run, followed by its arguments.
    #[arg(last = true, required = true)]
    command: Vec<std::ffi::OsString>,
}

#[derive(Args)]
struct MonthlyArgs {
    #[arg(long)]
    socket: PathBuf,
    /// One account. Allowed only for a configured administrator, or when it is
    /// the caller's own uid.
    #[arg(long)]
    only_uid: Option<u32>,
    /// Earliest month, `YYYY-MM`.
    #[arg(long)]
    from_month: Option<String>,
    /// Latest month, `YYYY-MM`.
    #[arg(long)]
    to_month: Option<String>,
    #[arg(long, value_enum, default_value_t = ReportFormat::Text)]
    format: ReportFormat,
}

#[derive(Args)]
struct RetainArgs {
    /// The live database. Opened directly, so run this as the service account
    /// that owns it.
    #[arg(long)]
    database: PathBuf,
    /// Days of individual events to keep. Older ones become monthly totals.
    #[arg(long, default_value_t = DEFAULT_KEEP_DAYS)]
    keep_days: u32,
    /// The daemon's socket. Checked first: the daemon is the only writer, and
    /// swapping the file under it would leave it writing where nothing reads.
    /// Omit only when you have stopped it by other means and know it is down.
    #[arg(long)]
    socket: Option<PathBuf>,
    /// Report what would change, and change nothing.
    #[arg(long)]
    dry_run: bool,
    #[arg(long, value_enum, default_value_t = ReportFormat::Text)]
    format: ReportFormat,
}

#[derive(Args)]
struct RetireUidArgs {
    /// The live database. Opened directly, so run this as the service account
    /// that owns it.
    #[arg(long)]
    database: PathBuf,
    /// The uid whose account has been removed.
    #[arg(long)]
    uid: u32,
    /// Who held it, recorded so the log is readable later.
    #[arg(long)]
    username: Option<String>,
    /// Why, in a few words.
    #[arg(long)]
    note: Option<String>,
    /// When the account was removed, in Unix seconds. Defaults to now. Events
    /// recorded after this belong to the next generation of the uid.
    #[arg(long)]
    at: Option<i64>,
}

#[derive(Args)]
struct ReplayArgs {
    #[arg(long)]
    socket: PathBuf,
    /// Spool to drain. Defaults to this account's own, as an attempt would use.
    #[arg(long)]
    spool: Option<PathBuf>,
    /// Events to attempt in this pass.
    #[arg(long, default_value_t = 256)]
    batch: usize,
}

#[derive(Args)]
struct BackupArgs {
    /// The live database. Opened directly, so run this as the service account
    /// that owns it; a snapshot is safe while the daemon is writing.
    #[arg(long)]
    database: PathBuf,
    /// Directory the dated backup is published into, created mode 0700.
    #[arg(long)]
    into: PathBuf,
    /// Backups to keep. Older ones are removed after a successful publish.
    #[arg(long, default_value_t = 14)]
    keep: usize,
}

#[derive(Args)]
struct RestoreArgs {
    /// The backup to put back.
    #[arg(long)]
    from: PathBuf,
    /// The database to replace. The existing file is kept alongside, renamed.
    #[arg(long)]
    to: PathBuf,
    /// Describe what restoring would do, and change nothing.
    #[arg(long)]
    dry_run: bool,
    /// Required to replace an existing database, since a restore discards every
    /// event acknowledged after the backup was taken.
    #[arg(long)]
    force: bool,
}

#[derive(Args)]
struct ReportArgs {
    #[arg(long)]
    socket: PathBuf,
    /// Total one account. Allowed only for a configured administrator, or when
    /// it is the caller's own uid.
    #[arg(long)]
    only_uid: Option<u32>,
    /// A UTC month (`2026-09`) or day (`2026-09-26`). Boundaries are UTC so the
    /// same work is never attributed to different days for different readers.
    #[arg(long, conflicts_with_all = ["since", "until"])]
    period: Option<String>,
    /// Earliest `occurred_at`, in Unix seconds.
    #[arg(long)]
    since: Option<i64>,
    /// Latest `occurred_at`, in Unix seconds.
    #[arg(long)]
    until: Option<i64>,
    #[arg(long, value_enum, default_value_t = ReportFormat::Text)]
    format: ReportFormat,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum ReportFormat {
    Text,
    Json,
    Csv,
}

#[derive(Args)]
struct DaemonArgs {
    /// Unix socket to listen on. Its directory must be operator-owned.
    #[arg(long)]
    socket: PathBuf,
    /// SQLite database file. Created if missing.
    #[arg(long)]
    database: PathBuf,
    /// A principal allowed to read every account. Repeatable. This is operator
    /// policy: it is never inferred from the caller or taken from a payload.
    #[arg(long = "admin-uid")]
    admin_uids: Vec<u32>,
    /// Mode for the socket file. Access is by group, set on the directory.
    #[arg(long, default_value = "660")]
    socket_mode: String,
    #[arg(long, default_value_t = 50)]
    max_requests_per_second: u32,
}

#[derive(Args)]
struct AppendArgs {
    #[arg(long)]
    socket: PathBuf,
}

#[derive(Args)]
struct QueryArgs {
    #[arg(long)]
    socket: PathBuf,
    /// Read one principal's rows. Allowed only for a configured administrator,
    /// or when it is the caller's own uid.
    #[arg(long)]
    only_uid: Option<u32>,
    /// Earliest `occurred_at`, in Unix seconds.
    #[arg(long)]
    since: Option<i64>,
    /// Latest `occurred_at`, in Unix seconds.
    #[arg(long)]
    until: Option<i64>,
    #[arg(long)]
    limit: Option<u32>,
}

#[derive(Subcommand)]
enum AttemptCommand {
    /// Run one attempt. The task is read from stdin; the report is printed as JSON.
    Run(RunArgs),
}

#[derive(Args)]
struct RunArgs {
    /// Private directory for stdout.log and stderr.log (created with mode 0700).
    #[arg(long)]
    output_dir: PathBuf,
    /// Wall-clock limit for the attempt, in seconds.
    #[arg(long)]
    deadline_secs: u64,
    /// Identifies the run this attempt belongs to. Generated when omitted.
    #[arg(long)]
    run_id: Option<String>,
    /// Identifies this attempt within the run. A retry is a new attempt.
    #[arg(long, default_value = "1")]
    attempt_id: String,
    /// Preserves an existing usage event id instead of deriving one from the
    /// role and identifiers.
    #[arg(long)]
    client_event_id: Option<String>,
    /// File the model was told to write its final message to, read after the
    /// attempt. Timon does not pass this to the child.
    #[arg(long)]
    result_file: Option<PathBuf>,
    /// JSON Schema the model was given for the result file. Without it the
    /// result is treated as free-form text.
    #[arg(long, requires = "result_file")]
    result_schema: Option<PathBuf>,
    /// Where to read usage events from: none, stdout, stderr, or a file path.
    #[arg(long, default_value = "none")]
    usage_source: String,
    /// Whether each usage event reports its own turn or a running total.
    #[arg(long, value_enum, default_value_t = UsageAccounting::Delta)]
    usage_accounting: UsageAccounting,
    /// Maximum task size in bytes.
    #[arg(long, default_value_t = timon::worker::DEFAULT_MAX_TASK_BYTES)]
    max_task_bytes: usize,
    /// Maximum bytes captured per output stream.
    #[arg(long, default_value_t = timon::worker::DEFAULT_MAX_OUTPUT_BYTES)]
    max_output_bytes: u64,
    /// Directory holding the concurrency slot files.
    #[arg(long, requires = "slots")]
    slot_dir: Option<PathBuf>,
    /// Number of concurrency slots.
    #[arg(long, requires = "slot_dir")]
    slots: Option<NonZeroU16>,
    /// Require slot files to exist instead of creating them (shared hosts).
    #[arg(long, requires = "slot_dir")]
    provisioned_slots: bool,
    /// Working directory for the attempt.
    #[arg(long)]
    cwd: Option<PathBuf>,
    /// Report this attempt's usage to the recorder listening here. Without it
    /// nothing is reported and no spool is written.
    #[arg(long)]
    usage_socket: Option<PathBuf>,
    /// Private spool directory. Defaults to $TIMON_SPOOL_DIR, else
    /// $XDG_STATE_HOME/timon/usage-spool, else ~/.local/state/timon/usage-spool.
    #[arg(long)]
    usage_spool: Option<PathBuf>,
    /// Provider label recorded with the event.
    #[arg(long)]
    provider: Option<String>,
    /// Model label recorded with the event.
    #[arg(long)]
    model: Option<String>,
    /// Spooled events to try to deliver after this attempt.
    #[arg(long, default_value_t = 32)]
    usage_replay_batch: usize,
    /// Directory holding this run's shared admission ledger. Without it the run
    /// has no allowance and nothing is admitted or recorded.
    #[arg(long, requires = "run_id")]
    run_ledger: Option<PathBuf>,
    /// Attempts this run may start. Enforced: the ledger is where they start.
    #[arg(long, default_value_t = 64)]
    run_max_attempts: u32,
    /// Token admission ceiling for the run. An estimate that decides whether to
    /// start another attempt, not a cap on what a running attempt spends.
    #[arg(long)]
    run_token_ceiling: Option<u64>,
    /// Tokens held for this attempt until its real usage is known. Set it from
    /// observed per-attempt usage on your own routes; it is never zero.
    #[arg(long, default_value_t = timon::admission::DEFAULT_ATTEMPT_RESERVE)]
    attempt_reserve: u64,
    /// Check the typed result's cited claims against the pages they cite.
    ///
    /// Fetches each cited URL from this host. Only use it where reaching the
    /// open internet from here is acceptable.
    #[arg(long, requires = "result_file")]
    verify_research: bool,
    /// Treat an unsupported or unverifiable claim as an unusable result.
    #[arg(long, requires = "verify_research")]
    require_supported_claims: bool,
    /// Whole-check deadline per claim.
    #[arg(long, default_value_t = 20, requires = "verify_research")]
    verify_timeout_secs: u64,
    /// Program to run, followed by its arguments.
    #[arg(last = true, required = true, num_args = 1..)]
    command: Vec<OsString>,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum UsageAccounting {
    /// Each event reports the tokens of that turn alone; events are summed.
    Delta,
    /// Each event reports the running total; the last event wins.
    Cumulative,
}

impl From<UsageAccounting> for Accumulation {
    fn from(value: UsageAccounting) -> Self {
        match value {
            UsageAccounting::Delta => Accumulation::PerTurnDelta,
            UsageAccounting::Cumulative => Accumulation::CumulativeSnapshot,
        }
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("timon: {error:#}");
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<u8> {
    let cli = Cli::parse();
    let (role, AttemptCommand::Run(args)) = match cli.command {
        Command::Lead(command) => (Role::Lead, command),
        Command::Worker(command) => (Role::Worker, command),
        Command::Usage(command) => return usage_command(command),
        Command::Slots(SlotsCommand::Provision(args)) => return slots_provision(args),
        Command::Research(ResearchCommand::Verify(args)) => return research_verify(args),
        Command::Mcp(args) => return mcp_serve(args),
        Command::Bridge(args) => return bridge_appserver(args),
        Command::Broker(BrokerCommand::Accounts(args)) => return broker_accounts(args),
        Command::Broker(BrokerCommand::Serve(args)) => return broker_serve(args),
        Command::Broker(BrokerCommand::Refresh(args)) => return broker_refresh(args),
        Command::Orchestrate(args) => return orchestrate_run(args),
        Command::Run(args) => return run_start(args),
        Command::Handoff(args) => return handoff_serve(args),
        Command::Triage(args) => return triage_show(args),
        Command::Runs(RunsCommand::List(args)) => return runs_list(args),
        Command::Runs(RunsCommand::Show(args)) => return runs_show(args),
        Command::Runs(RunsCommand::Cancel(args)) => return runs_cancel(args),
        Command::Runs(RunsCommand::Recover(args)) => return runs_recover(args),
    };
    attempt_run(role, args)
}

fn orchestrate_run(args: OrchestrateArgs) -> Result<u8> {
    let goal = if args.goal == "-" {
        let mut buffer = String::new();
        std::io::stdin()
            .lock()
            .take(args.max_task_bytes as u64)
            .read_to_string(&mut buffer)
            .context("failed to read the goal from stdin")?;
        buffer
    } else {
        args.goal.clone()
    };
    if goal.trim().is_empty() {
        bail!("the goal is empty");
    }
    let slots = match (&args.slot_dir, args.slots) {
        (Some(dir), Some(limit)) => Some(SlotPool::new(
            dir,
            limit,
            if args.provisioned_slots {
                SlotMode::Provisioned
            } else {
                SlotMode::CreateMissing
            },
        )),
        _ => None,
    };
    let ledger = match &args.run_ledger {
        Some(dir) => Some(
            Ledger::open(dir, &args.run_id)
                .with_context(|| format!("opening {}", dir.display()))?,
        ),
        None => None,
    };
    let parse_command = |json: &str, what: &str| -> Result<Vec<OsString>> {
        let parts: Vec<String> = serde_json::from_str(json)
            .with_context(|| format!("--{what} must be a JSON array of strings, got {json}"))?;
        if parts.is_empty() {
            bail!("--{what} is empty");
        }
        Ok(parts.into_iter().map(OsString::from).collect())
    };
    let lead_command = parse_command(&args.lead_command, "lead-command")?;
    let worker_command = parse_command(&args.worker_command, "worker-command")?;

    let phases = timon::orchestrate::Phases {
        run_id: args.run_id,
        goal,
        lead_command,
        worker_command,
        output_root: args.output_root,
        lead_deadline: Duration::from_secs(args.lead_deadline_secs),
        worker_deadline: Duration::from_secs(args.worker_deadline_secs),
        max_tasks: args.max_tasks,
        max_task_bytes: args.max_task_bytes,
        max_output_bytes: args.max_output_bytes,
        slots,
        ledger,
        limits: RunLimits {
            max_attempts: args.run_max_attempts,
            token_ceiling: args.run_token_ceiling,
            attempt_reserve: args.attempt_reserve,
        },
        usage_source: parse_usage_source(&args.usage_source),
        accumulation: args.usage_accounting.into(),
        deliverable_schema: args.deliverable_schema,
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to start the async runtime")?;
    let outcome = runtime.block_on(timon::orchestrate::run(&phases))?;
    println!("{}", serde_json::to_string_pretty(&outcome)?);
    if let Some(reason) = &outcome.stopped {
        eprintln!("timon: {reason}");
    }
    // Non-zero when the run did not produce an answer, so a caller that only
    // checks the status is not told a partial run succeeded.
    Ok(if outcome.answer.is_some() {
        0
    } else {
        EXIT_ATTEMPT_FAILED
    })
}

fn mcp_serve(args: McpArgs) -> Result<u8> {
    let slots = match (&args.slot_dir, args.slots) {
        (Some(dir), Some(limit)) => Some(SlotPool::new(
            dir,
            limit,
            if args.provisioned_slots {
                SlotMode::Provisioned
            } else {
                SlotMode::CreateMissing
            },
        )),
        _ => None,
    };
    let ledger = match &args.run_ledger {
        Some(dir) => Some(
            Ledger::open(dir, &args.run_id)
                .with_context(|| format!("opening {}", dir.display()))?,
        ),
        None => None,
    };
    let policy = WorkerPolicy {
        command: args.command,
        run_id: args.run_id,
        output_root: args.output_root,
        deadline: Duration::from_secs(args.worker_deadline_secs),
        max_task_bytes: args.max_task_bytes,
        max_output_bytes: args.max_output_bytes,
        slots,
        ledger,
        limits: RunLimits {
            max_attempts: args.run_max_attempts,
            token_ceiling: args.run_token_ceiling,
            attempt_reserve: args.attempt_reserve,
        },
        usage_source: parse_usage_source(&args.usage_source),
        accumulation: args.usage_accounting.into(),
        result_file: args.result_file,
        result_schema: args.result_schema,
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to start the async runtime")?;
    runtime.block_on(timon::mcp::server::serve(policy))?;
    Ok(0)
}

fn research_verify(args: VerifyArgs) -> Result<u8> {
    let json = if args.findings == "-" {
        let mut buffer = String::new();
        std::io::stdin()
            .lock()
            .take(MAX_REQUEST_BYTES as u64)
            .read_to_string(&mut buffer)
            .context("failed to read findings from stdin")?;
        buffer
    } else {
        std::fs::read_to_string(&args.findings)
            .with_context(|| format!("reading {}", args.findings))?
    };
    let findings: Findings =
        serde_json::from_str(&json).context("the findings could not be parsed")?;
    let report = verify::check(&findings, Duration::from_secs(args.timeout_secs));

    match args.format {
        VerifyFormat::Json => println!("{}", serde_json::to_string_pretty(&report)?),
        VerifyFormat::Text => print!("{}", render_verification(&report)),
    }
    // Non-zero unless every sourced claim was shown to be supported. An
    // unverifiable claim is not a pass: a route that cannot be checked must not
    // let claims through by being unavailable.
    Ok(if report.all_sourced_claims_quoted {
        0
    } else {
        1
    })
}

/// Renders a verification for a person.
fn render_verification(report: &verify::Report) -> String {
    let mut out = String::new();
    for checked in &report.checked {
        let (label, detail) = match &checked.verdict {
            verify::Verdict::QuotationPresent { final_url, .. } => {
                ("quote found ", final_url.clone())
            }
            verify::Verdict::Unsupported { reason } => ("UNSUPPORTED", reason.clone()),
            verify::Verdict::Unverifiable { reason } => ("unverifiable", reason.clone()),
            verify::Verdict::NotChecked => {
                ("inference  ", "not checked; cites nothing".to_string())
            }
        };
        let claim = checked.claim.text.chars().take(72).collect::<String>();
        out.push_str(&format!("  {label}  {claim}\n                 {detail}\n"));
    }
    out.push_str(&format!(
        "\n  {} quote found, {} unsupported, {} unverifiable -> {}\n",
        report.quotation_present,
        report.unsupported,
        report.unverifiable,
        if report.all_sourced_claims_quoted {
            "every sourced claim's quotation was found on its page"
        } else {
            "NOT every sourced claim's quotation was found"
        }
    ));
    out.push_str(&format!("\n  {}\n", report.basis));
    out
}

fn slots_provision(args: ProvisionArgs) -> Result<u8> {
    let octal = |value: &str, what: &str| {
        u32::from_str_radix(value, 8).with_context(|| format!("--{what} {value} is not octal"))
    };
    let report = timon::worker::slots::provision(
        &args.dir,
        args.slots,
        args.group.as_deref(),
        octal(&args.dir_mode, "dir-mode")?,
        octal(&args.file_mode, "file-mode")?,
    )
    .context("the slots could not be provisioned")?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    if !report.extra_left_in_place.is_empty() {
        eprintln!(
            "timon: {} slot file(s) beyond --slots were left in place; remove them deliberately \
when nothing is running, or a live worker would lose its slot",
            report.extra_left_in_place.len()
        );
    }
    Ok(0)
}

fn usage_command(command: UsageCommand) -> Result<u8> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to start the async runtime")?;
    match command {
        UsageCommand::Daemon(args) => runtime.block_on(usage_daemon(args)),
        UsageCommand::Append(args) => runtime.block_on(usage_append(args)),
        UsageCommand::Query(args) => runtime.block_on(usage_query(args)),
        UsageCommand::Report(args) => runtime.block_on(usage_report(args)),
        UsageCommand::Replay(args) => runtime.block_on(usage_replay(args)),
        UsageCommand::Backup(args) => usage_backup(args),
        UsageCommand::Restore(args) => usage_restore(args),
        UsageCommand::Monthly(args) => runtime.block_on(usage_monthly(args)),
        UsageCommand::Retain(args) => usage_retain(args),
        UsageCommand::RetireUid(args) => usage_retire_uid(args),
    }
}

fn broker_accounts(args: BrokerAccountsArgs) -> Result<u8> {
    let store = broker::store::Store::open(&args.store).map_err(|error| {
        anyhow::anyhow!(
            "{error}. Create it as the service account that will own the pooled \
             credentials, mode 0700, and log in once per account with CODEX_HOME \
             pointed at a subdirectory of it."
        )
    })?;
    let inventory =
        broker::store::Inventory::of(&store).map_err(|error| anyhow::anyhow!("{error}"))?;
    match args.format {
        ReportFormat::Json => println!("{}", serde_json::to_string_pretty(&inventory)?),
        // CSV would be a third rendering to keep credential-free; text and JSON
        // are enough for an inventory, so it reuses the text form rather than
        // adding a surface that has to be audited.
        ReportFormat::Text | ReportFormat::Csv => {
            print!("{}", broker::store::render(&inventory))
        }
    }
    // Exit 1 when something in the store is faulty, so a provisioning script can
    // notice without parsing the output.
    Ok(if inventory.accounts.iter().all(|a| a.usable()) {
        0
    } else {
        1
    })
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs() as i64)
        .unwrap_or_default()
}

fn open_runs(store: Option<PathBuf>) -> Result<timon::run::record::Runs> {
    let path = store.unwrap_or_else(default_run_store);
    timon::run::record::Runs::open(&path)
        .map_err(|error| anyhow::anyhow!("opening the run store {}: {error}", path.display()))
}

/// Renders one run for a person.
fn render_run(run: &timon::run::record::Run) -> String {
    let mut out = String::new();
    out.push_str(&format!("{}  {}\n", run.id, run.status.as_str()));
    out.push_str(&format!("  goal        {}\n", run.goal));
    out.push_str(&format!("  principal   uid {}\n", run.principal_uid));
    if let Some(workspace) = &run.workspace {
        out.push_str(&format!("  workspace   {}\n", workspace.display()));
    }
    out.push_str(&format!("  base        {}\n", run.base.describe()));
    let accounts = if run.accounts.is_empty() {
        "any the broker chooses".to_string()
    } else {
        run.accounts.join(", ")
    };
    out.push_str(&format!("  accounts    {accounts}\n"));
    out.push_str(&format!("  attempts    up to {}\n", run.max_attempts));
    match run.token_ceiling {
        Some(ceiling) => out.push_str(&format!(
            "  ceiling     {ceiling} tokens (admission only, not a cap)\n"
        )),
        None => out.push_str("  ceiling     none set\n"),
    }
    if let Some(deadline) = run.deadline {
        out.push_str(&format!(
            "  deadline    {}\n",
            timon::recorder::render::utc(deadline)
        ));
    }
    out.push_str(&format!(
        "  started     {}\n",
        timon::recorder::render::utc(run.started_at)
    ));
    if let Some(ended) = run.ended_at {
        out.push_str(&format!(
            "  ended       {}\n",
            timon::recorder::render::utc(ended)
        ));
    }
    if let Some(detail) = &run.detail {
        out.push_str(&format!("  note        {detail}\n"));
    }
    out
}

fn run_start(args: HandoffArgs) -> Result<u8> {
    use timon::run::start::{Request, admit};

    let runs = open_runs(args.store)?;
    let workspace = match args.workspace {
        Some(path) => Some(path),
        None => std::env::current_dir().ok(),
    };
    let base = timon::run::start::base_of(workspace.as_deref());
    let now = now_secs();

    let request = Request {
        goal: args.goal,
        // From the kernel, not from a flag: attribution a caller can set is not
        // attribution.
        principal_uid: nix_uid(),
        workspace,
        accounts: args.accounts,
        max_attempts: args.max_attempts,
        token_ceiling: args.token_ceiling,
        deadline: args.deadline_secs.map(|secs| now + secs),
        submission_key: args.submission_key,
    };

    let run = match admit(
        &runs,
        request,
        base,
        now,
        None,
        timon::admission::DEFAULT_ATTEMPT_RESERVE,
    ) {
        Ok(run) => run,
        Err(refused) => {
            eprintln!("timon run: not started. {refused}");
            return Ok(1);
        }
    };

    // Triage is deterministic, so it happens here rather than costing a model
    // call to decide what a model call would cost.
    let decision = timon::triage::decide(
        &run.goal,
        &timon::triage::Allowances {
            route: args.route.as_deref().and_then(timon::triage::Route::parse),
            allow_planner: args.allow_planner,
        },
    );
    if let Err(error) = runs.record_triage(&run.id, &decision, now) {
        // Recorded or not, the run exists. Losing the reasons costs a later
        // measurement, not this run, so it is reported rather than fatal.
        eprintln!("timon run: the triage decision could not be recorded: {error}");
    }

    match args.format {
        ReportFormat::Csv => {
            eprintln!("csv is for usage exports; a run record is reported as text or json");
            return Ok(2);
        }
        ReportFormat::Json => println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "run": run,
                "triage": decision,
            }))?
        ),
        ReportFormat::Text => {
            print!("{}", render_run(&run));
            println!("  route       {}", decision.summary());
            println!(
                "\nRecorded. This run outlives the terminal: `timon runs show {}` \n\
                 reports it later, and `timon runs cancel {}` stops it.",
                run.id, run.id
            );
            if args.preflight_only {
                println!(
                    "\nPreflight only. Triage and the pipeline are P2 onwards; nothing \n\
                     has been sent to a model and no quota has been spent."
                );
            }
        }
    }
    Ok(0)
}

/// This process's real uid.
fn nix_uid() -> u32 {
    // SAFETY: `getuid` cannot fail and touches no memory the caller owns.
    unsafe { libc::getuid() }
}

/// Shows how a goal would be routed.
///
/// Exists so the routing instrument can ask the shipped binary rather than
/// reimplement the rules. An instrument that carries its own copy of what it
/// measures is measuring the copy.
fn triage_show(args: TriageArgs) -> Result<u8> {
    let decision = timon::triage::decide(
        &args.goal,
        &timon::triage::Allowances {
            route: args.route.as_deref().and_then(timon::triage::Route::parse),
            allow_planner: args.allow_planner,
        },
    );
    match args.format {
        ReportFormat::Csv => {
            eprintln!("csv is for usage exports; a decision is reported as text or json");
            return Ok(2);
        }
        ReportFormat::Json => println!("{}", serde_json::to_string_pretty(&decision)?),
        ReportFormat::Text => {
            println!("{}", decision.summary());
            for reason in &decision.reasons {
                println!("  · {} ({})", reason.detail, reason.rule);
            }
        }
    }
    Ok(0)
}

/// Serves the hand-off tool for one developer's Codex session.
///
/// Runs as that developer, which is what makes attribution the kernel's answer:
/// the uid is this process's own, and a session cannot claim to be somebody
/// else by asking nicely.
fn handoff_serve(args: HandoffServeArgs) -> Result<u8> {
    let workspace = match args.workspace {
        Some(path) => Some(path),
        None => std::env::current_dir().ok(),
    };
    let policy = timon::mcp::handoff::HandoffPolicy {
        store: args.store.unwrap_or_else(default_run_store),
        workspace_root: workspace,
        default_max_attempts: args.default_max_attempts,
        max_attempts_limit: args.max_attempts_limit,
        attempt_reserve: timon::admission::DEFAULT_ATTEMPT_RESERVE,
        principal_uid: nix_uid(),
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(timon::mcp::server::serve_handoff(policy))?;
    // The stdin pump can be parked on a blocking read that `abort` cannot
    // interrupt, so the runtime is dropped without waiting for it. Learned the
    // hard way: the bridge hung on exit for exactly this reason.
    runtime.shutdown_timeout(std::time::Duration::from_millis(0));
    Ok(0)
}

fn runs_list(args: RunsListArgs) -> Result<u8> {
    let runs = open_runs(args.store)?;
    let recent = runs
        .recent(args.limit)
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    match args.format {
        ReportFormat::Csv => {
            eprintln!("csv is for usage exports; run records are reported as text or json");
            return Ok(2);
        }
        ReportFormat::Json => println!("{}", serde_json::to_string_pretty(&recent)?),
        ReportFormat::Text => {
            if recent.is_empty() {
                println!("No runs yet. `timon run \"<goal>\"` starts one.");
                return Ok(0);
            }
            for run in &recent {
                println!(
                    "{:<28} {:<12} {}",
                    run.id,
                    run.status.as_str(),
                    run.goal.lines().next().unwrap_or("")
                );
            }
        }
    }
    Ok(0)
}

fn runs_show(args: RunsShowArgs) -> Result<u8> {
    let runs = open_runs(args.store)?;
    let run = match runs.get(&args.id) {
        Ok(run) => run,
        Err(error) => {
            eprintln!("{error}");
            return Ok(1);
        }
    };
    let triage = runs.triage_of(&run.id).unwrap_or_default();
    match args.format {
        ReportFormat::Csv => {
            eprintln!("csv is for usage exports; a run record is reported as text or json");
            return Ok(2);
        }
        ReportFormat::Json => println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "run": run,
                "triage": triage,
            }))?
        ),
        ReportFormat::Text => {
            print!("{}", render_run(&run));
            if let Some(decision) = triage.first() {
                println!("  route       {}", decision.summary());
                // Every rule that fired, because one line cannot carry why a
                // route was chosen over the two that were not.
                for reason in &decision.reasons {
                    println!("              · {} ({})", reason.detail, reason.rule);
                }
            }
        }
    }
    Ok(0)
}

fn runs_cancel(args: RunsCancelArgs) -> Result<u8> {
    let runs = open_runs(args.store)?;
    match runs.cancel(&args.id, nix_uid(), now_secs()) {
        Ok(run) => {
            println!("{}: {}", run.id, run.status.as_str());
            Ok(0)
        }
        Err(error) => {
            eprintln!("{error}");
            Ok(1)
        }
    }
}

fn runs_recover(args: RunsRecoverArgs) -> Result<u8> {
    let runs = open_runs(args.store)?;
    let stranded = runs
        .interrupt_stale(now_secs())
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    match args.format {
        ReportFormat::Csv => {
            eprintln!("csv is for usage exports; run records are reported as text or json");
            return Ok(2);
        }
        ReportFormat::Json => println!("{}", serde_json::to_string_pretty(&stranded)?),
        ReportFormat::Text => {
            if stranded.is_empty() {
                println!("No runs were left behind.");
            } else {
                println!(
                    "{} run(s) were still marked running and cannot be; marked interrupted:",
                    stranded.len()
                );
                for run in &stranded {
                    println!("  {}  {}", run.id, run.goal.lines().next().unwrap_or(""));
                }
                println!(
                    "\nTheir work stopped when the orchestrator did. Nothing was resumed, \n\
                     and nothing was silently retried."
                );
            }
        }
    }
    Ok(0)
}

/// Renews pooled access tokens on demand.
///
/// Exists because the broker's own refresh is driven by the clock, and the clock
/// is not the only thing that ends a token. A provider can invalidate one while
/// it still has days left on it — a subscription change does exactly that — and
/// nothing in the credential file shows it. This is the operator's way to recover
/// an account without a full re-login, and the honest answer when it cannot.
fn broker_refresh(args: BrokerRefreshArgs) -> Result<u8> {
    let store = broker::store::Store::open(&args.store).map_err(|e| anyhow::anyhow!("{e}"))?;
    let accounts = match &args.account {
        Some(name) => vec![store.read(name)],
        None => store
            .accounts()
            .map_err(|e| anyhow::anyhow!("{e}"))?
            .into_iter()
            .collect(),
    };
    if accounts.is_empty() {
        bail!("no accounts in {}", args.store.display());
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs() as i64)
        .unwrap_or_default();

    let mut failures = 0;
    for account in &accounts {
        if !account.refreshable {
            println!(
                "{}: no refresh token; this account needs a fresh login",
                account.name
            );
            failures += 1;
            continue;
        }
        // Read only to decide whether renewal is due. The value goes no further.
        let due = match broker::store::credential_of(account) {
            Ok(credential) => broker::refresh::due(&credential.bearer, now),
            Err(error) => {
                println!("{}: cannot read the credential: {error}", account.name);
                failures += 1;
                continue;
            }
        };
        if !due && !args.force {
            println!(
                "{}: not near expiry, left alone. Use --force if the provider is \
refusing it anyway.",
                account.name
            );
            continue;
        }
        match broker::refresh::refresh(&account.home) {
            Ok(expiry) => println!(
                "{}: renewed, now valid until {}",
                account.name,
                timon::recorder::render::utc(expiry)
            ),
            Err(error) => {
                println!("{}: could not renew: {error}", account.name);
                failures += 1;
            }
        }
    }

    if failures > 0 {
        eprintln!(
            "\n{failures} account(s) could not be renewed. An account whose refresh \
token is also invalid has to be logged in again, as the user that owns it:\n  \
CODEX_HOME={}/<account> codex login",
            args.store.display()
        );
        return Ok(1);
    }
    Ok(0)
}

fn broker_serve(args: BrokerServeArgs) -> Result<u8> {
    use std::sync::Arc;

    let listen: std::net::SocketAddr = args
        .listen
        .parse()
        .with_context(|| format!("--listen {} is not an address", args.listen))?;
    if !listen.ip().is_loopback() {
        // Refused rather than warned. This process holds credentials for
        // everyone who uses it; something reachable off the host would let
        // anyone who can route to it spend that quota, and the caller-identity
        // check depends on the peer being local.
        bail!(
            "--listen {listen} is not a loopback address. The broker holds pooled \
             credentials and identifies callers by their local uid, neither of \
             which survives being reachable from the network."
        );
    }

    let store = broker::store::Store::open(&args.store).map_err(|e| anyhow::anyhow!("{e}"))?;
    let inventory = broker::store::Inventory::of(&store).map_err(|e| anyhow::anyhow!("{e}"))?;

    // Checked before binding, so a pool that cannot serve anything is a refusal
    // to start rather than a listener that refuses every request.
    let serving: Vec<String> = match &args.account {
        Some(only) => {
            let account = store.read(only);
            if !account.usable() {
                bail!(
                    "account {only:?} cannot serve requests:\n  - {}",
                    account.faults.join("\n  - ")
                );
            }
            vec![account.name]
        }
        None => inventory
            .accounts
            .iter()
            .filter(|account| account.usable())
            .map(|account| account.name.clone())
            .collect(),
    };
    if serving.is_empty() {
        bail!(
            "no pooled account in {} can serve requests. `timon broker accounts` \
             says what is wrong with each.",
            args.store.display()
        );
    }

    let config = Arc::new(broker::serve::Config {
        listen,
        upstream: args
            .upstream
            .unwrap_or_else(|| broker::serve::DEFAULT_UPSTREAM.to_string()),
        store,
        serving: serving.clone(),
        models: broker::policy::ModelPolicy {
            assign: args.assign_model.clone(),
            allowed: args.allow_models.clone(),
            version: args.model_policy_version.clone(),
        },
        pool: std::sync::Mutex::new(broker::select::Pool::new()),
        grants: std::sync::Mutex::new(broker::grant::Grants::new()),
        read_timeout: std::time::Duration::from_secs(args.read_timeout_secs),
    });
    let counters = Arc::new(broker::serve::Counters::default());
    let listener =
        std::net::TcpListener::bind(listen).with_context(|| format!("binding {listen}"))?;

    eprintln!(
        "timon broker: serving {} account(s) [{}] on {} -> {}",
        serving.len(),
        serving.join(", "),
        listen,
        config.upstream
    );
    eprintln!(
        "  callers are identified by uid from the kernel; a request whose caller \
cannot be identified is refused, never attributed to a default."
    );
    if serving.len() > 1 {
        eprintln!(
            "  a conversation stays on the account that started it; a new one goes \
to whichever account has the most of its window left."
        );
    }
    match &args.assign_model {
        Some(model) => eprintln!(
            "  model policy {:?}: sessions get {model}. A request that asks for \
something else is served by {model} and told so on the response.",
            args.model_policy_version
        ),
        None => {
            eprintln!("  no model policy: requests are forwarded with whatever model they ask for.")
        }
    }
    // Check in with the supervisor, if there is one. `ready()` is what releases
    // `Type=notify`, so ordering only completes once the port is actually open.
    broker::notify::ready();
    start_watchdog(Arc::clone(&config), Arc::clone(&counters));

    broker::serve::serve(config, counters, listener, Arc::new(|| false))?;
    Ok(0)
}

/// Checks in with systemd for as long as the broker can actually serve.
///
/// Deliberately a dead-man's switch rather than a report. The failure worth
/// catching here is a poisoned pool lock, which leaves the process running and
/// the port open while every request fails; a supervisor watching for a crash
/// would never act on it. Stopping the check-in is what causes the restart.
///
/// Does nothing when not running under systemd.
fn start_watchdog(
    config: std::sync::Arc<broker::serve::Config>,
    counters: std::sync::Arc<broker::serve::Counters>,
) {
    let Some(interval) = broker::notify::interval() else {
        return;
    };
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(interval);
            let health = broker::health::check(&config, &counters);
            if health.healthy {
                broker::notify::status(&format!(
                    "serving {} of {} account(s); {} request(s) forwarded",
                    health.accounts_usable, health.accounts_configured, health.requests_forwarded
                ));
                broker::notify::watchdog();
            } else {
                // Said once into the journal, so the restart has a reason
                // attached rather than appearing as an unexplained kill.
                let reason = health.faults.join("; ");
                eprintln!("timon broker: not healthy, withholding watchdog: {reason}");
                broker::notify::degraded(&reason);
            }
        }
    });
}

fn bridge_appserver(args: BridgeArgs) -> Result<u8> {
    if bridge::appserver::looks_like_shared_daemon(&args.command) {
        // Refused rather than warned. A shared daemon serves several accounts
        // from one process, so every session would be attributed to whoever
        // started it, and a report that confidently names the wrong person is
        // worse than no report at all.
        bail!(
            "that command runs or attaches to a shared app-server daemon, so the recorder \
             would see the daemon's identity rather than each person's. Run an app-server \
             per account instead: `timon bridge -- codex app-server`"
        );
    }

    let spool = match args.socket.as_ref() {
        None => None,
        Some(_) => {
            let dir = args
                .spool
                .clone()
                .or_else(Spool::resolve)
                .context("no spool directory: pass --spool or set TIMON_SPOOL_DIR")?;
            Some(Spool::open(dir, DEFAULT_MAX_SPOOLED_EVENTS).context("opening the spool")?)
        }
    };

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("starting the runtime")?;
    let outcome = runtime.block_on(bridge::appserver::run(bridge::appserver::Config {
        command: args.command,
        socket: args.socket,
        spool,
        model: args.model,
    }));
    // Shut the runtime down without waiting for its blocking pool. `tokio::io::stdin`
    // reads on a blocking thread, and aborting the task that owns it does not
    // interrupt a read already in progress; dropping the runtime normally would
    // then wait for that thread and hang the process after the child has exited.
    // Everything that had to finish already has: the child is reaped, its output
    // was flushed as it was forwarded, and the recorder task was awaited inside
    // `run`.
    runtime.shutdown_timeout(std::time::Duration::from_millis(0));
    let report = outcome?;

    if args.report {
        // To stderr: stdout carries the protocol and must stay byte-identical.
        eprintln!("{}", serde_json::to_string_pretty(&report)?);
    }
    Ok(report.exit_code.unwrap_or(0).clamp(0, 255) as u8)
}

async fn usage_monthly(args: MonthlyArgs) -> Result<u8> {
    let response = recorder_send(
        &args.socket,
        &Request::Monthly {
            from_month: args.from_month,
            to_month: args.to_month,
            only_uid: args.only_uid,
        },
    )
    .await?;
    match response {
        Response::Monthly { months, scope_uid } => {
            match args.format {
                ReportFormat::Text => print!("{}", render::monthly_text(&months, scope_uid)),
                ReportFormat::Csv => print!("{}", render::monthly_csv(&months)),
                ReportFormat::Json => println!("{}", serde_json::to_string_pretty(&months)?),
            }
            Ok(0)
        }
        other => print_response(&other),
    }
}

fn usage_retain(args: RetainArgs) -> Result<u8> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default();
    let report = match retain::retain(
        &args.database,
        args.keep_days,
        now,
        args.socket.as_deref(),
        args.dry_run,
    ) {
        Ok(report) => report,
        // Exit 75 for a live daemon: the work was refused for a reason that will
        // pass, which is temporary failure rather than misuse.
        Err(error @ retain::RetainError::DaemonLive(_)) => {
            eprintln!("timon: {error}");
            return Ok(75);
        }
        Err(error) => return Err(anyhow::anyhow!("{error}")),
    };
    match args.format {
        ReportFormat::Text | ReportFormat::Csv => {
            print!("{}", render::retention_text(&report, args.dry_run))
        }
        ReportFormat::Json => println!("{}", serde_json::to_string_pretty(&report)?),
    }
    Ok(0)
}

fn usage_retire_uid(args: RetireUidArgs) -> Result<u8> {
    let at = args.at.unwrap_or_else(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or_default()
    });
    let mut store = Store::open(&args.database)
        .with_context(|| format!("opening {}", args.database.display()))?;
    let recorded = store
        .retire_principal(args.uid, args.username.as_deref(), args.note.as_deref(), at)
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    println!(
        "uid {} retired at {}; generation {} is closed and new events belong to generation {}",
        recorded.peer_uid,
        render::utc(recorded.retired_at),
        recorded.generation,
        recorded.generation + 1
    );
    println!(
        "Existing rows are untouched. Reports for this uid no longer total the closed \n\
         generation together with the next one, and an administrator can still see both."
    );
    Ok(0)
}

async fn usage_daemon(args: DaemonArgs) -> Result<u8> {
    let mode = u32::from_str_radix(&args.socket_mode, 8)
        .with_context(|| format!("--socket-mode {} is not octal", args.socket_mode))?;
    let mut config = RecorderConfig::new(args.socket, args.database);
    config.admin_uids = args.admin_uids;
    config.socket_mode = mode;
    config.max_requests_per_second = args.max_requests_per_second;
    serve_recorder(config, shutdown_signal()).await?;
    Ok(0)
}

async fn usage_append(args: AppendArgs) -> Result<u8> {
    let mut json = String::new();
    std::io::stdin()
        .lock()
        .take(MAX_REQUEST_BYTES as u64 + 1)
        .read_to_string(&mut json)
        .context("failed to read the event from stdin")?;
    if json.len() > MAX_REQUEST_BYTES {
        bail!("event exceeds {MAX_REQUEST_BYTES} bytes");
    }
    // Parsed only to fail fast on an obviously wrong event. The original text is
    // what gets sent, so the daemon records exactly what the producer wrote.
    let event: UsageEvent = serde_json::from_str(&json).context("event is not a usage event")?;
    event.validate().context("event cannot be recorded")?;
    let request = serde_json::json!({ "type": "append", "event": serde_json::from_str::<serde_json::Value>(&json)? });
    let response = recorder_send_line(&args.socket, &request.to_string()).await?;
    print_response(&response)
}

async fn usage_query(args: QueryArgs) -> Result<u8> {
    let response = recorder_send(
        &args.socket,
        &Request::Query {
            since: args.since,
            until: args.until,
            only_uid: args.only_uid,
            limit: args.limit,
        },
    )
    .await?;
    print_response(&response)
}

async fn usage_report(args: ReportArgs) -> Result<u8> {
    let (since, until) = match &args.period {
        Some(spec) => {
            let (from, to) = render::period(spec).map_err(|error| anyhow::anyhow!(error))?;
            (Some(from), Some(to))
        }
        None => (args.since, args.until),
    };
    let response = recorder_send(
        &args.socket,
        &Request::Report {
            since,
            until,
            only_uid: args.only_uid,
        },
    )
    .await?;
    match response {
        Response::Report { report } => {
            match args.format {
                ReportFormat::Text => print!("{}", render::text(&report)),
                ReportFormat::Csv => print!("{}", render::csv(&report)),
                ReportFormat::Json => println!("{}", serde_json::to_string_pretty(&report)?),
            }
            Ok(0)
        }
        other => print_response(&other),
    }
}

async fn usage_replay(args: ReplayArgs) -> Result<u8> {
    let dir = args
        .spool
        .or_else(Spool::resolve)
        .context("no spool directory: pass --spool or set TIMON_SPOOL_DIR")?;
    let spool = Spool::open(dir, DEFAULT_MAX_SPOOLED_EVENTS).context("opening the spool")?;
    let outcome = replay(&args.socket, &spool, args.batch)
        .await
        .context("the spool could not be read")?;

    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "delivered": outcome.delivered,
            "already_present": outcome.already_present,
            "still_pending": outcome.still_pending,
            "corrupt": outcome.corrupt,
            "notes": outcome.notes,
            "gap": spool.read_gap().ok().flatten(),
        }))?
    );
    if outcome.corrupt > 0 {
        eprintln!(
            "timon: {} spooled event(s) were unreadable and set aside; those tokens are \
unknown, not zero",
            outcome.corrupt
        );
    }
    // Anything still pending means the recorder refused or is unreachable, which
    // a caller draining a spool needs to notice.
    Ok(if outcome.still_pending > 0 { 1 } else { 0 })
}

fn usage_backup(args: BackupArgs) -> Result<u8> {
    let store = Store::open(&args.database)
        .with_context(|| format!("opening {}", args.database.display()))?;
    let record = store
        .backup(&args.into, args.keep)
        .context("the backup was not published")?;
    println!("{}", serde_json::to_string_pretty(&record)?);
    eprintln!(
        "timon: backed up {} event(s) to {}; recovery point is row {}",
        record.events,
        record.path.display(),
        record
            .recovery_point_id
            .map(|id| id.to_string())
            .unwrap_or_else(|| "(none)".to_string())
    );
    Ok(0)
}

fn usage_restore(args: RestoreArgs) -> Result<u8> {
    if args.dry_run {
        let plan = plan_restore(&args.from, &args.to).context("the backup did not verify")?;
        println!("{}", serde_json::to_string_pretty(&plan)?);
        eprintln!("timon: {}", RESTORE_CAVEAT);
        return Ok(0);
    }
    if args.to.exists() && !args.force {
        // Refuse rather than ask: this discards acknowledged events, and a
        // prompt is not available when this runs from a timer or a script.
        bail!(
            "{} already exists. {} Pass --force to replace it, or --dry-run to see what would change.",
            args.to.display(),
            RESTORE_CAVEAT
        );
    }
    let plan = restore(&args.from, &args.to).context("the restore did not complete")?;
    println!("{}", serde_json::to_string_pretty(&plan)?);
    eprintln!("timon: {}", RESTORE_CAVEAT);
    if let Some(lost) = plan.live_rows_not_in_backup
        && lost > 0
    {
        eprintln!("timon: {lost} event(s) present before the restore are not in the backup.");
    }
    Ok(0)
}

/// Prints the daemon's reply and turns a refusal into a non-zero exit, so a
/// caller that only checks the status still notices a rejected event.
fn print_response(response: &Response) -> Result<u8> {
    println!("{}", serde_json::to_string_pretty(response)?);
    Ok(match response {
        Response::Error { .. } => EXIT_ATTEMPT_FAILED,
        _ => 0,
    })
}

fn attempt_run(role: Role, args: RunArgs) -> Result<u8> {
    // Kept before the spec takes ownership of the arguments it needs.
    let verify_research = args.verify_research;
    let require_supported_claims = args.require_supported_claims;
    let verify_timeout = Duration::from_secs(args.verify_timeout_secs);
    let result_file_for_verify = args.result_file.clone();

    let usage_source = parse_usage_source(&args.usage_source);
    let task = read_task(args.max_task_bytes)?;

    // Reserved before the attempt starts, and only from the ledger, so two
    // processes cannot both take the last of a run's allowance.
    let ledger = match (&args.run_ledger, &args.run_id) {
        (Some(dir), Some(run_id)) => {
            Some(Ledger::open(dir, run_id).with_context(|| format!("opening {}", dir.display()))?)
        }
        _ => None,
    };
    let attempt_id = args.attempt_id.clone();
    let admission_limits = RunLimits {
        max_attempts: args.run_max_attempts,
        token_ceiling: args.run_token_ceiling,
        attempt_reserve: args.attempt_reserve,
    };
    if let Some(ledger) = &ledger {
        match ledger
            .admit(&attempt_id, &admission_limits)
            .context("reading the run's admission ledger")?
        {
            Ok(admitted) => {
                eprintln!(
                    "timon: admitted, holding {} tokens for this attempt. {}",
                    admitted.reserved, admitted.basis
                );
            }
            Err(refusal) => {
                eprintln!("timon: not admitted: {refusal}");
                println!("{}", serde_json::to_string_pretty(&refusal)?);
                return Ok(EXIT_RUN_ALLOWANCE);
            }
        }
    }

    let _lease = match (&args.slot_dir, args.slots) {
        (Some(dir), Some(limit)) => {
            let mode = if args.provisioned_slots {
                SlotMode::Provisioned
            } else {
                SlotMode::CreateMissing
            };
            match SlotPool::new(dir, limit, mode).try_acquire() {
                Ok(lease) => Some(lease),
                Err(SlotError::Full) => {
                    eprintln!("timon: all worker slots are in use; retry later");
                    return Ok(EXIT_NO_SLOT);
                }
                Err(error) => return Err(error.into()),
            }
        }
        _ => None,
    };

    let mut command = args.command.into_iter();
    let program = PathBuf::from(command.next().context("program is missing")?);
    let mut limits = WorkerLimits::with_deadline(Duration::from_secs(args.deadline_secs));
    limits.max_task_bytes = args.max_task_bytes;
    limits.max_output_bytes = args.max_output_bytes;
    let spec = AttemptSpec {
        role,
        run_id: args.run_id.unwrap_or_else(generated_run_id),
        attempt_id: args.attempt_id,
        client_event_id: args.client_event_id,
        worker: WorkerSpec {
            program,
            args: command.collect(),
            cwd: args.cwd,
            env_set: Vec::new(),
            env_remove: Vec::new(),
            task,
            output_dir: args.output_dir,
            limits,
        },
        result_file: args.result_file,
        result_schema: args.result_schema,
        usage_source,
        accumulation: args.usage_accounting.into(),
    };

    let runtime = tokio::runtime::Runtime::new().context("failed to start async runtime")?;
    let report = runtime.block_on(run_attempt(&spec, shutdown_signal()))?;

    // Settled with whatever the attempt reported, including nothing. An unknown
    // total keeps its reservation rather than releasing tokens that may well
    // have been spent.
    if let Some(ledger) = &ledger {
        match ledger.settle(&attempt_id, report.usage.total.value()) {
            Ok(settled) if !settled.usage_known => eprintln!(
                "timon: this attempt reported no usage, so its {} reserved token(s) stay held \
against the run: unknown is not zero",
                admission_limits.attempt_reserve
            ),
            Ok(_) => {}
            Err(error) => eprintln!("timon: the admission ledger could not be settled: {error}"),
        }
    }

    // Reporting runs after the work and can only degrade tracking, never the
    // attempt: a recorder that is down must not fail a run the user asked for.
    let recording = args.usage_socket.as_deref().map(|socket| {
        runtime.block_on(record_usage(
            socket,
            args.usage_spool.clone(),
            &report,
            args.provider.as_deref(),
            args.model.as_deref(),
            args.usage_replay_batch,
        ))
    });
    if let Some(recording) = &recording
        && let Some(warning) = recording.warning()
    {
        eprintln!("timon: {warning}");
    }

    // Verified after the attempt, never during it: a claim check reaches the
    // network, and it must not be able to stall or fail the work itself.
    let verification = if verify_research {
        verify_attempt_research(result_file_for_verify.as_deref(), verify_timeout, &report)
    } else {
        None
    };
    if let Some(verification) = &verification
        && !verification.all_sourced_claims_quoted
    {
        eprintln!(
            "timon: {} of {} sourced claim(s) could not be matched to the pages they cite",
            verification.unsupported + verification.unverifiable,
            verification.quotation_present + verification.unsupported + verification.unverifiable
        );
    }

    println!(
        "{}",
        serde_json::to_string_pretty(&ReportedAttempt {
            attempt: &report,
            recording: recording.as_ref(),
            research: verification.as_ref(),
        })?
    );

    // A result whose sourced claims do not hold is a result that cannot be used,
    // which is what exit 65 already means.
    if require_supported_claims
        && verification
            .as_ref()
            .is_some_and(|v| !v.all_sourced_claims_quoted)
    {
        return Ok(EXIT_RESULT_UNUSABLE);
    }

    Ok(if report.process.cancelled {
        EXIT_CANCELLED
    } else if report.process.timed_out {
        EXIT_TIMED_OUT
    } else if !report.process.succeeded() {
        EXIT_ATTEMPT_FAILED
    } else if report.result.is_failure() {
        EXIT_RESULT_UNUSABLE
    } else {
        0
    })
}

/// An attempt, plus what became of its usage record.
#[derive(serde::Serialize)]
struct ReportedAttempt<'a> {
    #[serde(flatten)]
    attempt: &'a timon::attempt::AttemptReport,
    #[serde(skip_serializing_if = "Option::is_none")]
    recording: Option<&'a Recording>,
    #[serde(skip_serializing_if = "Option::is_none")]
    research: Option<&'a verify::Report>,
}

/// Checks the cited claims in an attempt's typed result, when it holds any.
///
/// A result that is not a findings document is not a failure: most attempts are
/// not research, and `--verify-research` on one of those simply has nothing to
/// check.
fn verify_attempt_research(
    result_file: Option<&std::path::Path>,
    timeout: Duration,
    report: &timon::attempt::AttemptReport,
) -> Option<verify::Report> {
    if !report.result.is_usable() {
        return None;
    }
    let path = result_file?;
    let json = std::fs::read_to_string(path).ok()?;
    let findings: Findings = serde_json::from_str(&json).ok()?;
    if findings.claims.is_empty() {
        return None;
    }
    Some(verify::check(&findings, timeout))
}

/// What happened when this attempt's usage was reported.
#[derive(serde::Serialize)]
struct Recording {
    /// `recorded`, `spooled` or `dropped`.
    outcome: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<i64>,
    /// The daemon already held this event; nothing was added.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    duplicate: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
    /// Events recovered from the spool during this attempt.
    #[serde(skip_serializing_if = "Option::is_none")]
    replayed: Option<ReplaySummary>,
    /// Events known to be missing entirely. Their tokens are unknown, not zero.
    #[serde(skip_serializing_if = "Option::is_none")]
    gap: Option<timon::recorder::producer::Gap>,
}

#[derive(serde::Serialize)]
struct ReplaySummary {
    delivered: u64,
    already_present: u64,
    still_pending: u64,
    corrupt: u64,
}

impl Recording {
    /// A line worth putting on stderr, where a person will see it.
    fn warning(&self) -> Option<String> {
        if self.outcome == "dropped" {
            return Some(format!(
                "usage for this attempt was NOT recorded: {}",
                self.detail.as_deref().unwrap_or("reason unknown")
            ));
        }
        if self.outcome == "spooled" {
            return Some(format!(
                "usage for this attempt is spooled, not yet recorded: {}",
                self.detail.as_deref().unwrap_or("reason unknown")
            ));
        }
        if let Some(gap) = &self.gap
            && gap.dropped > 0
        {
            return Some(format!(
                "usage tracking is degraded: {} event(s) were lost and cannot be recovered",
                gap.dropped
            ));
        }
        None
    }
}

/// Spools and delivers this attempt's usage, then drains what it can.
async fn record_usage(
    socket: &std::path::Path,
    spool_dir: Option<PathBuf>,
    report: &timon::attempt::AttemptReport,
    provider: Option<&str>,
    model: Option<&str>,
    replay_batch: usize,
) -> Recording {
    let Some(dir) = spool_dir.or_else(Spool::resolve) else {
        return Recording {
            outcome: "dropped",
            id: None,
            duplicate: false,
            detail: Some("no spool directory: set --usage-spool or TIMON_SPOOL_DIR".to_string()),
            replayed: None,
            gap: None,
        };
    };
    let spool = match Spool::open(dir, DEFAULT_MAX_SPOOLED_EVENTS) {
        Ok(spool) => spool,
        Err(error) => {
            return Recording {
                outcome: "dropped",
                id: None,
                duplicate: false,
                detail: Some(format!("the spool could not be opened: {error}")),
                replayed: None,
                gap: None,
            };
        }
    };

    let event = event_for(report, provider, model);
    let delivery = deliver(socket, &spool, &event).await;

    // Only worth draining once this attempt's own event is through; otherwise
    // the recorder is down and a replay pass would just repeat the failure.
    let replayed = match &delivery {
        Delivery::Recorded { .. } => {
            replay(socket, &spool, replay_batch)
                .await
                .ok()
                .map(|r| ReplaySummary {
                    delivered: r.delivered,
                    already_present: r.already_present,
                    still_pending: r.still_pending,
                    corrupt: r.corrupt,
                })
        }
        _ => None,
    };

    let (outcome, id, duplicate, detail) = match delivery {
        Delivery::Recorded { id, duplicate } => ("recorded", Some(id), duplicate, None),
        Delivery::Spooled { reason } => ("spooled", None, false, Some(reason)),
        Delivery::Dropped { reason } => ("dropped", None, false, Some(reason)),
    };
    Recording {
        outcome,
        id,
        duplicate,
        detail,
        replayed,
        gap: spool
            .read_gap()
            .ok()
            .flatten()
            .filter(|gap| gap.dropped > 0),
    }
}

/// Reads `--usage-source`. Anything that is not a known keyword is a path, so a
/// file literally named `stdout` has to be given as `./stdout`.
fn parse_usage_source(value: &str) -> UsageSource {
    match value {
        "none" => UsageSource::None,
        "stdout" => UsageSource::Stdout,
        "stderr" => UsageSource::Stderr,
        path => UsageSource::File {
            path: PathBuf::from(path),
        },
    }
}

/// A run id for a single ad-hoc attempt. A real run supplies `--run-id`, so
/// that every attempt in it shares one.
fn generated_run_id() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis())
        .unwrap_or(0);
    format!("run-{millis}-{}", std::process::id())
}

fn read_task(limit: usize) -> Result<String> {
    let mut bytes = Vec::new();
    std::io::stdin()
        .lock()
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .context("failed to read task from stdin")?;
    if bytes.len() > limit {
        bail!("task exceeds {limit} bytes");
    }
    String::from_utf8(bytes).context("task is not valid UTF-8")
}

async fn shutdown_signal() {
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        () = terminate => {}
    }
}
