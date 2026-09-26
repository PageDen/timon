use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use std::ffi::OsString;
use std::io::Read;
use std::num::NonZeroU16;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use timon::attempt::{AttemptSpec, Role, UsageSource, run_attempt};
use timon::recorder::client::{send as recorder_send, send_line as recorder_send_line};
use timon::recorder::db::{RESTORE_CAVEAT, Store, plan_restore, restore};
use timon::recorder::event::{MAX_REQUEST_BYTES, UsageEvent};
use timon::recorder::producer::{
    DEFAULT_MAX_SPOOLED_EVENTS, Delivery, Spool, deliver, event_for, replay,
};
use timon::recorder::protocol::{Request, Response};
use timon::recorder::render;
use timon::recorder::server::{Config as RecorderConfig, serve as serve_recorder};
use timon::usage::Accumulation;
use timon::worker::slots::{SlotError, SlotMode, SlotPool};
use timon::worker::{WorkerLimits, WorkerSpec};

/// Exit code when no worker slot is free (EX_TEMPFAIL).
const EXIT_NO_SLOT: u8 = 75;
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
    /// Take a verified snapshot of the database.
    Backup(BackupArgs),
    /// Put a verified snapshot back in place.
    Restore(RestoreArgs),
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
    };
    attempt_run(role, args)
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
        UsageCommand::Backup(args) => usage_backup(args),
        UsageCommand::Restore(args) => usage_restore(args),
    }
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
    let usage_source = parse_usage_source(&args.usage_source);
    let task = read_task(args.max_task_bytes)?;

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

    println!(
        "{}",
        serde_json::to_string_pretty(&ReportedAttempt {
            attempt: &report,
            recording: recording.as_ref(),
        })?
    );

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
