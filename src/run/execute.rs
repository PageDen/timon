// Adapted for Timon. Not derived from Prodex source.
//! Running the route triage chose.
//!
//! The first thing in this project that calls a model on its own initiative.
//! Everything before it was deliberately inert, so the care here is about what
//! that costs and who it is charged to.
//!
//! Three things happen before a worker starts, and the order matters:
//!
//! 1. **The broker issues a grant** naming the run, the accounts it may spend
//!    and the model its route chose. Authority comes from the broker because the
//!    broker is what checks it (P1.4).
//! 2. **The grant reaches the worker through its environment**, and the worker's
//!    provider configuration turns it into a request header. Verified against
//!    Codex 0.155.1: `env_http_headers` puts the value on the wire.
//! 3. **The worker is told nothing else.** It gets a task on stdin and a
//!    provider to talk to. It does not know which account pays, cannot choose
//!    one, and never sees a credential.
//!
//! A run that cannot get a grant does not start. Running without one would mean
//! spending quota that no run is accountable for, which is the thing the whole
//! credential design exists to prevent.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::run::record::{Run, RunError, Runs, Status};
use crate::triage::{Decision, Route};

/// The environment variable a worker finds its grant in.
///
/// Matched by the provider configuration's `env_http_headers`, which is what
/// turns it into `x-timon-grant` on the request.
pub const GRANT_ENV: &str = "TIMON_GRANT";

/// The sandbox a worker runs under. Always stated, never inherited.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Sandbox {
    /// Can read the repository, cannot change it.
    ReadOnly,
    /// Can change files in its own worktree and nowhere else. P4.3 qualified it.
    WorkspaceWrite,
}

impl Sandbox {
    pub fn as_str(self) -> &'static str {
        match self {
            Sandbox::ReadOnly => "read-only",
            Sandbox::WorkspaceWrite => "workspace-write",
        }
    }
}

/// The `codex exec` command a worker runs, pointed at the broker.
///
/// Two things are set here on the command line rather than left to whatever
/// the developer's Codex configuration says, and both were found by looking at
/// a real run's transcript rather than by any test:
///
/// **The provider.** A worker was handed its grant in `TIMON_GRANT` and the
/// design assumed a provider in the developer's config would carry it to the
/// broker. Nothing installed one. So every worker talked straight to OpenAI on
/// the developer's own login, the broker forwarded nothing, and no run was ever
/// paid from the pooled accounts — `requests_forwarded: 0` after a run, and
/// `provider: openai` in its transcript. Now the provider is passed with the
/// command: the worker sends its own login and the grant, and the broker strips
/// the login and pays from a pooled account.
///
/// **The sandbox.** A worker given no `-s` inherits the developer's default,
/// which on the host this was found on was `danger-full-access`. The run that
/// wrote a file had the whole disk; it stayed in its worktree only because
/// that was where it started. Every worker now states its sandbox.
///
/// The task still goes on stdin, never in the arguments, where every account on
/// the host could read it from a process listing.
pub fn worker_command(
    broker: &str,
    model: Option<&str>,
    sandbox: Sandbox,
) -> Vec<std::ffi::OsString> {
    let mut command: Vec<std::ffi::OsString> = vec!["codex".into(), "exec".into()];
    if let Some(model) = model {
        command.push("-m".into());
        command.push(model.into());
    }
    command.push("-s".into());
    command.push(sandbox.as_str().into());
    // Network off for the commands a worker runs, whatever the developer's
    // config says. The host this was found on had `network_access = true` under
    // `[sandbox_workspace_write]`, so workers could reach the network while the
    // qualification record said `reach_git_remote: blocked` — the qualified state
    // was not the state workers ran in. Model calls are made by Codex itself,
    // outside the sandbox, so a worker loses nothing it needs.
    command.push("-c".into());
    command.push("sandbox_workspace_write.network_access=false".into());
    for setting in provider_settings(broker) {
        command.push("-c".into());
        command.push(setting.into());
    }
    command.push("--skip-git-repo-check".into());
    command.push("-".into());
    command
}

/// The provider settings that send a worker's model calls to the broker.
///
/// `requires_openai_auth` because the upstream is the ChatGPT backend, which
/// expects that request shape; the login the worker sends is stripped by the
/// broker and replaced with a pooled account's, so it never reaches the
/// provider. `env_http_headers` turns the grant into `x-timon-grant`.
pub fn provider_settings(broker: &str) -> Vec<String> {
    vec![
        format!("model_provider=\"{PROVIDER}\""),
        format!("model_providers.{PROVIDER}.name=\"Timon broker\""),
        format!("model_providers.{PROVIDER}.base_url=\"http://{broker}\""),
        format!("model_providers.{PROVIDER}.wire_api=\"responses\""),
        format!("model_providers.{PROVIDER}.requires_openai_auth=true"),
        format!(
            "model_providers.{PROVIDER}.env_http_headers={{\"{}\"=\"{GRANT_ENV}\"}}",
            crate::broker::grant::GRANT_HEADER
        ),
    ]
}

/// The provider name a worker's transcript reports when it is routed correctly.
pub const PROVIDER: &str = "timon";

/// Why a run could not be executed.
#[derive(Debug)]
pub enum ExecuteError {
    /// The broker would not issue a grant. Without one nothing starts.
    NoGrant(String),
    /// The broker could not be reached at all.
    Unreachable(String),
    Store(RunError),
    /// The route is understood but not implemented yet.
    NotImplemented(Route),
    /// The planner route failed before any task ran.
    Planning(String),
}

impl std::fmt::Display for ExecuteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExecuteError::NoGrant(why) => write!(
                f,
                "the broker would not authorise this run: {why}. Nothing was started, \
                 because work nobody is accountable for is worse than work that did \
                 not happen"
            ),
            ExecuteError::Unreachable(why) => write!(
                f,
                "the broker could not be reached: {why}. It holds the credentials, so \
                 there is nowhere else for this run to go"
            ),
            ExecuteError::Store(error) => write!(f, "{error}"),
            ExecuteError::NotImplemented(route) => {
                write!(f, "the {} route is not built yet", route.as_str())
            }
            ExecuteError::Planning(detail) => write!(f, "{detail}"),
        }
    }
}

impl std::error::Error for ExecuteError {}

/// What a run may spend, as the broker granted it.
#[derive(Debug, Serialize)]
pub struct Granted {
    /// Kept out of `Serialize` by hand below: this is the secret.
    #[serde(skip)]
    pub token: String,
    pub id: String,
    pub expires_at: i64,
    pub accounts: Vec<String>,
    pub model: Option<String>,
}

impl Granted {
    /// The environment a worker is started with.
    ///
    /// One variable. The worker learns what it may spend against, and nothing
    /// about how.
    pub fn environment(&self) -> Vec<(String, String)> {
        vec![(GRANT_ENV.to_string(), self.token.clone())]
    }
}

/// Asks the broker to authorise this run.
///
/// Over the same loopback listener that serves requests, so the caller is
/// identified by uid from the kernel exactly as a request is, and a grant can
/// only ever be issued to whoever asked for it.
pub fn authorise(
    broker: &str,
    run: &Run,
    model: Option<&str>,
    lifetime_secs: i64,
) -> Result<Granted, ExecuteError> {
    let body = serde_json::json!({
        "run_id": run.id,
        "accounts": run.accounts,
        "model": model,
        "lifetime_secs": lifetime_secs,
    })
    .to_string();

    let response = post(broker, "/_timon/grant", &body)
        .map_err(|error| ExecuteError::Unreachable(error.to_string()))?;

    let value: serde_json::Value = serde_json::from_str(&response)
        .map_err(|_| ExecuteError::NoGrant(format!("unreadable reply: {response}")))?;
    if let Some(error) = value.get("error") {
        let message = error
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("refused");
        return Err(ExecuteError::NoGrant(message.to_string()));
    }
    let token = value
        .get("grant")
        .and_then(|g| g.as_str())
        .ok_or_else(|| ExecuteError::NoGrant("the reply carried no grant".to_string()))?;

    Ok(Granted {
        token: token.to_string(),
        id: value
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string(),
        expires_at: value
            .get("expires_at")
            .and_then(|v| v.as_i64())
            .unwrap_or(0),
        accounts: run.accounts.clone(),
        model: model.map(str::to_string),
    })
}

/// Ends a run's authority, whatever happened to the run.
///
/// Called on every path out, including failure. A grant left live after its run
/// has stopped is authority nobody is watching.
/// The terminal status for a run that has stopped running.
///
/// `Status::Cancelled` existed and nothing ever set it: every path out of
/// execution settled `Finished`, so a run stopped by Ctrl-C or by `timon runs
/// cancel` was recorded as having completed. `timon runs list` then printed
/// `finished` beside work nobody received, which is the kind of report this
/// project exists not to produce.
///
/// Public so the rule can be tested on its own, like `refresh::apply` and
/// `watch::look`. Every settle path out of execution goes through it, so a
/// future path that forgets is the thing to watch for, not the rule itself.
pub fn terminal(cancel: &std::sync::atomic::AtomicBool) -> Status {
    if cancel.load(std::sync::atomic::Ordering::Relaxed) {
        Status::Cancelled
    } else {
        Status::Finished
    }
}

pub fn revoke(broker: &str, run_id: &str) {
    let body = serde_json::json!({ "run_id": run_id, "revoke": true }).to_string();
    if let Err(error) = post(broker, "/_timon/grant", &body) {
        // Reported, not fatal: the grant expires on its own, and failing a
        // finished run because cleanup did not answer helps nobody.
        eprintln!("timon: the grant for {run_id} could not be revoked now: {error}");
    }
}

/// A minimal HTTP POST to the broker's loopback listener.
///
/// Hand-rolled rather than pulling in a client: the broker is on loopback, the
/// bodies are small JSON, and the one thing that matters is that this uses the
/// same socket a request would, so the kernel reports the same uid.
fn post(broker: &str, path: &str, body: &str) -> std::io::Result<String> {
    use std::io::{Read, Write};

    let address = broker.trim_start_matches("http://");
    let mut stream = std::net::TcpStream::connect(address)?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(30)))?;
    let request = format!(
        "POST {path} HTTP/1.1\r\nhost: {address}\r\ncontent-type: application/json\r\n\
         content-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes())?;
    stream.flush()?;
    let mut raw = String::new();
    stream.read_to_string(&mut raw)?;
    Ok(raw
        .split_once("\r\n\r\n")
        .map(|(_, body)| body.to_string())
        .unwrap_or(raw))
}

/// What executing a run produced.
#[derive(Debug, Serialize)]
pub struct Executed {
    pub run_id: String,
    pub route: Route,
    /// The branch the worker's changes landed on, when it made any.
    pub branch: Option<String>,
    /// The account-facing identity of the authority this ran under.
    pub grant_id: String,
    pub status: Status,
    /// The worker's own output, as it produced it.
    pub output: Option<String>,
    pub reported_tokens: Option<u64>,
    pub detail: Option<String>,
    /// The sandbox the worker actually ran under.
    pub sandbox: Sandbox,
}

/// How a run is executed. None of it is the model's to choose.
pub struct Plan {
    /// Where the broker listens.
    pub broker: String,
    /// Set to stop the run. Watched by the worker, not only by the caller:
    /// stopping the caller while a child keeps talking to a provider is not
    /// cancelling, it is losing track.
    pub cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// The command for a cheap worker, and for a strong one. Fixed by the
    /// operator: a model that could choose its own command is not bounded by
    /// anything the command enforces.
    pub cheap_command: Vec<std::ffi::OsString>,
    pub strong_command: Vec<std::ffi::OsString>,
    /// The command for a task that changes files.
    ///
    /// Separate because it needs the write sandbox, and because that is a
    /// permission rather than a detail: a run where the reading and writing
    /// commands are the same is a run where everything can write.
    pub write_command: Vec<std::ffi::OsString>,
    /// The writing command for a single cheap worker. The strong one is
    /// `write_command`.
    pub cheap_write_command: Vec<std::ffi::OsString>,
    /// Whether this host's write sandbox is qualified. A single-worker route
    /// writes only when it is, which is the rule the planner route already
    /// followed: absent qualification means shut, not assumed.
    pub writing_permitted: bool,
    /// The models each route runs on. Fixed by the operator, like the commands.
    pub cheap_model: Option<String>,
    pub strong_model: Option<String>,
    pub output_root: PathBuf,
    pub deadline: std::time::Duration,
    pub max_output_bytes: u64,
    /// How long the grant lives. Bounded again by the broker.
    pub grant_lifetime_secs: i64,
}

impl Route {
    /// The model this route runs on, as the operator configured it.
    ///
    /// Sent to the broker in the grant, so the run gets the model triage chose
    /// rather than whatever the worker's own configuration happens to say. The
    /// broker treats it as authoritative over its interactive session policy,
    /// because triage picked it for this task.
    fn model_for(self, plan: &Plan) -> Option<&str> {
        match self {
            Route::CheapWorker => plan.cheap_model.as_deref(),
            Route::StrongWorker => plan.strong_model.as_deref(),
            Route::Planner => None,
        }
    }
}

impl Plan {
    fn command_for(&self, route: Route) -> Option<&Vec<std::ffi::OsString>> {
        match route {
            Route::CheapWorker => Some(&self.cheap_command),
            Route::StrongWorker => Some(&self.strong_command),
            Route::Planner => None,
        }
    }

    fn write_command_for(&self, route: Route) -> Option<&Vec<std::ffi::OsString>> {
        match route {
            Route::CheapWorker => Some(&self.cheap_write_command),
            Route::StrongWorker => Some(&self.write_command),
            Route::Planner => None,
        }
    }
}

/// Runs one goal through the route triage chose.
pub async fn execute(
    runs: &Runs,
    run: &Run,
    decision: &Decision,
    plan: &Plan,
    now: i64,
) -> Result<Executed, ExecuteError> {
    let Some(command) = plan.command_for(decision.route) else {
        // The planner route is carried out by `run::pipeline`, which needs a
        // repository and a graph. It is reached through `execute_planned`
        // rather than here, because the two share almost nothing beyond the
        // grant: one runs a worker, the other runs a plan.
        return Err(ExecuteError::NotImplemented(decision.route));
    };

    // Authority first. A run that cannot be authorised does not start, so there
    // is no path where a worker spends quota outside a grant.
    let granted = authorise(
        &plan.broker,
        run,
        decision.route.model_for(plan),
        plan.grant_lifetime_secs,
    )?;

    // Every worker with a repository works in its own worktree, whatever its
    // command is allowed to do. Deciding by whether the command *looks* like it
    // can write would mean parsing somebody's flags and being wrong once; this
    // way a misconfigured command can still only damage a throwaway checkout.
    //
    // A read-only worker loses nothing by it: the worktree starts from the run's
    // recorded base, snapshot included, so it sees the developer's real state
    // and the report can say exactly which commit that was.
    // Made before anything writes into it. Creating a subdirectory first leaves
    // the parent at whatever the umask says, and the worker refuses an output
    // directory that group or others can reach — correctly, since it holds a
    // developer's prompt and a model's reply.
    private_dir(&plan.output_root.join(&run.id)).map_err(ExecuteError::Planning)?;

    let mut workspace = match (&run.workspace, run.base.start_from()) {
        (Some(repository), Some(start)) => Some(
            crate::worktree::Workspace::open(
                repository,
                &run.id,
                start,
                plan.output_root.join(&run.id).join("worktrees"),
            )
            .map_err(|error| ExecuteError::Planning(error.to_string()))?,
        ),
        // No repository: research and other work with nothing to isolate.
        _ => None,
    };

    let worktree = match workspace.as_mut() {
        Some(workspace) => Some(
            workspace
                .worktree_for(decision.route.as_str())
                .map_err(|error| ExecuteError::Planning(error.to_string()))?,
        ),
        None => None,
    };

    // A single worker writes only into its own worktree, and only on a host
    // whose write sandbox is qualified — the rule the planner route already
    // followed. Without both, it runs read-only: it can still answer, and the
    // report says why it could not change anything.
    let (command, sandbox) = match plan.write_command_for(decision.route) {
        Some(writing) if worktree.is_some() && plan.writing_permitted => {
            (writing, Sandbox::WorkspaceWrite)
        }
        _ => (command, Sandbox::ReadOnly),
    };

    let outcome = run_worker(
        run,
        command,
        plan,
        &granted,
        worktree.as_ref().map(|tree| tree.path.as_path()),
    )
    .await;

    // Whatever it changed becomes a branch, so a single-worker route hands back
    // the same thing the planner route does: something to review, never an edit
    // to the developer's files.
    let branch = match (&workspace, &worktree) {
        (Some(workspace), Some(tree)) => match workspace.commit_work(tree) {
            Ok(Some(_)) => Some(tree.branch.clone()),
            _ => None,
        },
        _ => None,
    };

    // The run's authority ends with the run, on every path out of here.
    revoke(&plan.broker, &run.id);

    let (status, detail) = match &outcome {
        Ok(_) => (terminal(&plan.cancel), None),
        Err(why) => (terminal(&plan.cancel), Some(why.clone())),
    };
    runs.settle(&run.id, status, now, detail.as_deref())
        .map_err(ExecuteError::Store)?;

    Ok(Executed {
        run_id: run.id.clone(),
        route: decision.route,
        branch,
        grant_id: granted.id,
        status,
        output: outcome.as_ref().ok().and_then(|o| o.output.clone()),
        reported_tokens: outcome.as_ref().ok().and_then(|o| o.tokens),
        detail,
        sandbox,
    })
}

struct WorkerOutput {
    output: Option<String>,
    tokens: Option<u64>,
}

/// Asks the planner for a graph, then carries it out.
///
/// The same authority as any other run: a grant first, and nothing starts
/// without one. The planner is a worker like the others — it gets a task on
/// stdin and returns text — so the only thing special about it is what is done
/// with what it says.
/// Everything that bounds a run, together.
///
/// Passed as one thing because they are one thing: the budget derives the
/// others, and three separate parameters was three chances for a caller to pass
/// a set that does not agree with itself.
pub struct Envelope {
    pub limits: crate::dag::Limits,
    pub bounds: crate::dag_run::Bounds,
    pub budget: crate::budget::Budget,
}

pub async fn execute_planned(
    runs: &Runs,
    run: &Run,
    decision: &Decision,
    plan: &Plan,
    envelope: &Envelope,
    now: i64,
) -> Result<crate::run::pipeline::PipelineReport, ExecuteError> {
    let limits = &envelope.limits;
    let budget = &envelope.budget;
    use crate::run::pipeline::{Pipeline, accept_within, carry_out, parse_plan, plan_prompt};

    let Some(repository) = run.workspace.clone() else {
        return Err(ExecuteError::Planning(
            crate::run::pipeline::PipelineError::NoWorkspace.to_string(),
        ));
    };

    let granted = authorise(
        &plan.broker,
        run,
        decision.route.model_for(plan),
        plan.grant_lifetime_secs,
    )?;

    // The planner runs on the strong command: choosing how to split work is the
    // judgement the cheap route exists to avoid paying for.
    let asked = plan_prompt(&run.goal, limits);
    let planned = worker_session(
        run.workspace.as_deref(),
        &plan.strong_command,
        &asked,
        &granted.token,
        plan.deadline,
        &plan.output_root.join(&run.id).join("planner"),
        &plan.cancel,
    )
    .await;

    let answer = match planned {
        Ok(output) => output,
        Err(why) => {
            revoke(&plan.broker, &run.id);
            let _ = runs.settle(&run.id, terminal(&plan.cancel), now, Some(&why));
            return Err(ExecuteError::Planning(why));
        }
    };

    let parsed = parse_plan(&answer).and_then(|plan| accept_within(&plan, limits, budget));
    let graph = match parsed {
        Ok(graph) => graph,
        Err(error) => {
            revoke(&plan.broker, &run.id);
            let detail = error.to_string();
            let _ = runs.settle(&run.id, terminal(&plan.cancel), now, Some(&detail));
            return Err(ExecuteError::Planning(detail));
        }
    };

    let workspace = crate::worktree::Workspace::open(
        &repository,
        &run.id,
        run.base.commit().unwrap_or("HEAD"),
        plan.output_root.join(&run.id).join("worktrees"),
    )
    .map_err(|error| ExecuteError::Planning(error.to_string()))?;

    // The scheduler is synchronous — a task is a child process and it spends
    // its life waiting — but worker supervision is async, and that supervision
    // is what reaps a worker's process group. P4.3's "no process outlives its
    // worker" check holds because of it, so it is not something to drop for
    // the convenience of a sync call. The handle is captured here and the
    // scheduler runs on a blocking thread, so each worker thread can hand its
    // async work back to the runtime.
    let runner = WorktreeRunner {
        workspace: std::sync::Mutex::new(workspace),
        command: plan.strong_command.clone(),
        write_command: plan.write_command.clone(),
        grant: granted.token.clone(),
        deadline: plan.deadline,
        output_root: plan.output_root.join(&run.id),
        handle: tokio::runtime::Handle::current(),
    };
    let mut pipeline = Pipeline {
        runner: &runner,
        workspace: None,
        limits: *limits,
        bounds: envelope.bounds,
        cancel: std::sync::Arc::clone(&plan.cancel),
    };

    let mut report = tokio::task::block_in_place(|| carry_out(run, &graph, &mut pipeline));

    // Integration needs the workspace the runner has been using, so it happens
    // after the graph rather than inside the pipeline's own copy.
    let finished: Vec<String> = report
        .graph
        .tasks
        .iter()
        .filter(|task| task.outcome.finished())
        .map(|task| task.label.clone())
        .collect();
    if !finished.is_empty() {
        report.result_branch = runner
            .workspace
            .lock()
            .ok()
            .and_then(|mut workspace| workspace.integrate(&finished).ok());
    }

    revoke(&plan.broker, &run.id);
    let _ = runs.settle(
        &run.id,
        terminal(&plan.cancel),
        now,
        if plan.cancel.load(std::sync::atomic::Ordering::Relaxed) {
            Some("the run was cancelled")
        } else if report.graph.not_run.is_empty() {
            None
        } else {
            Some("some tasks did not run")
        },
    );
    Ok(report)
}

/// Runs each task in its own worktree, which is what P4.2 provides.
struct WorktreeRunner {
    workspace: std::sync::Mutex<crate::worktree::Workspace>,
    command: Vec<std::ffi::OsString>,
    write_command: Vec<std::ffi::OsString>,
    grant: String,
    deadline: std::time::Duration,
    output_root: PathBuf,
    handle: tokio::runtime::Handle,
}

impl crate::dag_run::Runner for WorktreeRunner {
    fn run(
        &self,
        task: &crate::dag::Task,
        input: &crate::dag_inputs::Input,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<crate::dag_inputs::Artifact, String> {
        let tree = {
            let mut workspace = self.workspace.lock().map_err(|_| "workspace poisoned")?;
            workspace
                .worktree_for(&task.label)
                .map_err(|error| error.to_string())?
        };

        // A dependent writing task starts from the commit the host prepared, so
        // what it edits already contains its dependencies' work.
        if let crate::dag_inputs::Input::PreparedCommit { commit, .. } = input {
            let reset = std::process::Command::new("git")
                .arg("-C")
                .arg(&tree.path)
                .args(["reset", "--hard", "--quiet", commit])
                .status()
                .map_err(|error| error.to_string())?;
            if !reset.success() {
                return Err(format!("could not start {} from {commit}", task.label));
            }
        }

        let brief = brief_for(task, input);
        // Handed back to the runtime, so the worker keeps its supervision and
        // its process group is reaped when it ends.
        let cancel_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(
            cancel.load(std::sync::atomic::Ordering::Relaxed),
        ));
        // A task that changes files needs the write sandbox. Giving every task
        // that command would mean every task can write, so the permission
        // follows the plan's own declaration.
        let command = match task.access {
            crate::dag::Access::Write => &self.write_command,
            crate::dag::Access::Read => &self.command,
        };
        let outcome = self.handle.block_on(worker_session(
            Some(&tree.path),
            command,
            &brief,
            &self.grant,
            self.deadline,
            &self.output_root.join(&task.label),
            &cancel_flag,
        ))?;

        let committed = {
            let workspace = self.workspace.lock().map_err(|_| "workspace poisoned")?;
            workspace
                .commit_work(&tree)
                .map_err(|error| error.to_string())?
        };

        // A clean exit is not evidence of work. A worker asked to change files
        // can answer helpfully, exit 0 and change nothing — that happened the
        // first time this ran end to end, because the sandbox refused the write
        // and the model explained what it would have written. Reported as a
        // failure, with what the worker said, because the alternative is a
        // result branch that looks complete and contains nothing.
        if let Some(complaint) = changed_nothing(task.access, committed.is_none(), &outcome) {
            return Err(complaint);
        }

        Ok(crate::dag_inputs::Artifact::new(
            &task.label,
            1,
            committed.unwrap_or(outcome),
        ))
    }

    fn prepare(&self, merged: &[String]) -> Result<String, String> {
        self.workspace
            .lock()
            .map_err(|_| "workspace poisoned".to_string())?
            .prepare(merged)
            .map_err(|error| error.to_string())
    }
}

/// Creates a directory only its owner can reach.
///
/// Every directory in this run's output holds somebody's prompt and a model's
/// reply, so the mode is the point rather than a formality.
fn private_dir(path: &Path) -> Result<(), String> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    if !path.exists() {
        builder
            .create(path)
            .map_err(|error| format!("{}: {error}", path.display()))?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
    }
    Ok(())
}

/// Whether a writing task actually wrote, and what to say when it did not.
///
/// A clean exit is not evidence of work. The first end-to-end run of the
/// planner route produced three tasks that all reported done and a result
/// branch containing nothing: the sandbox had refused the writes and each
/// worker explained, helpfully and at length, what it *would* have written.
/// Exit code 0 throughout.
///
/// So a task that declared it changes files and changed none is a failure, and
/// it carries what the worker said — which in that case named the cause
/// precisely.
pub fn changed_nothing(
    access: crate::dag::Access,
    nothing_committed: bool,
    said: &str,
) -> Option<String> {
    if access != crate::dag::Access::Write || !nothing_committed {
        return None;
    }
    Some(format!(
        "this task was asked to change files and changed none. The worker said: {}",
        said.trim().lines().take(3).collect::<Vec<_>>().join(" ")
    ))
}

/// What a worker is told.
///
/// A task's own text, plus whatever its dependencies produced — inline, because
/// a worker starts fresh and cannot go and look.
fn brief_for(task: &crate::dag::Task, input: &crate::dag_inputs::Input) -> String {
    let mut brief = task.task.clone();
    if let crate::dag_inputs::Input::Artifacts { from } = input
        && !from.is_empty()
    {
        brief.push_str("\n\nWhat the work you depend on produced:\n");
        for artifact in from {
            brief.push_str(&format!(
                "\n--- {} ---\n{}\n",
                artifact.producer, artifact.content
            ));
        }
    }
    brief
}

/// Runs one worker session and returns what it said.
///
/// Used by the planner and by every task, so both get the same supervision, the
/// same grant handling and the same rule about where the task text goes.
///
/// The task goes on stdin and never into the arguments, so it cannot end up in
/// a process listing that every account on the host can read.
pub async fn worker_session(
    cwd: Option<&Path>,
    command: &[std::ffi::OsString],
    task: &str,
    grant: &str,
    deadline: std::time::Duration,
    output_dir: &Path,
    cancel: &std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<String, String> {
    use crate::worker::{WorkerLimits, WorkerSpec, run_worker as spawn};

    let (program, args) = command
        .split_first()
        .ok_or_else(|| "the worker command is empty".to_string())?;

    let spec = WorkerSpec {
        program: PathBuf::from(program),
        args: args.to_vec(),
        cwd: cwd.map(Path::to_path_buf),
        env_set: vec![(GRANT_ENV.into(), grant.into())],
        env_remove: Vec::new(),
        task: task.to_string(),
        output_dir: output_dir.to_path_buf(),
        limits: WorkerLimits::with_deadline(deadline),
    };
    spec.validate().map_err(|error| format!("{error}"))?;

    let watching = std::sync::Arc::clone(cancel);
    let outcome = spawn(&spec, async move {
        while !watching.load(std::sync::atomic::Ordering::Relaxed) {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    })
    .await
    .map_err(|error| format!("{error}"))?;

    if outcome.cancelled {
        return Err("the run was cancelled".to_string());
    }
    if !outcome.succeeded() {
        return Err(describe(&outcome));
    }
    std::fs::read_to_string(&outcome.stdout.path)
        .map_err(|error| format!("reading the worker's output: {error}"))
}

/// Runs one worker session with the run's grant in its environment.
///
/// The task goes on stdin and never into the arguments, so it cannot end up in
/// a process listing that every account on the host can read.
async fn run_worker(
    run: &Run,
    command: &[std::ffi::OsString],
    plan: &Plan,
    granted: &Granted,
    worktree: Option<&Path>,
) -> Result<WorkerOutput, String> {
    use crate::worker::{WorkerLimits, WorkerSpec, run_worker as spawn};

    let (program, args) = command
        .split_first()
        .ok_or_else(|| "the worker command is empty".to_string())?;

    let spec = WorkerSpec {
        program: PathBuf::from(program),
        args: args.to_vec(),
        // The worktree when there is one, so the developer's own files are not
        // what a worker is pointed at.
        cwd: worktree
            .map(Path::to_path_buf)
            .or_else(|| run.workspace.clone()),
        env_set: granted
            .environment()
            .into_iter()
            .map(|(key, value)| (key.into(), value.into()))
            .collect(),
        env_remove: Vec::new(),
        task: run.goal.clone(),
        output_dir: plan.output_root.join(&run.id),
        limits: WorkerLimits::with_deadline(plan.deadline),
    };
    spec.validate().map_err(|error| format!("{error}"))?;

    // Cancellation reaches the worker's process group, not just this function.
    // The flag is polled rather than awaited on a channel because the thing
    // setting it is a signal handler or another thread, and a poll is the
    // smallest mechanism that works from both.
    let cancel = std::sync::Arc::clone(&plan.cancel);
    let outcome = spawn(&spec, async move {
        while !cancel.load(std::sync::atomic::Ordering::Relaxed) {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    })
    .await
    .map_err(|error| format!("{error}"))?;

    if outcome.cancelled {
        return Err("the run was cancelled".to_string());
    }
    if !outcome.succeeded() {
        return Err(describe(&outcome));
    }

    let output = std::fs::read_to_string(&outcome.stdout.path).ok();
    Ok(WorkerOutput {
        output,
        tokens: None,
    })
}

/// Says what went wrong in the operator's terms rather than a struct dump.
fn describe(outcome: &crate::worker::WorkerOutcome) -> String {
    if outcome.timed_out {
        // Naming the lever matters: the per-worker deadline is derived from the
        // run budget, so the fix is almost always a larger --budget-secs rather
        // than anything about the task.
        return "the worker passed its deadline and was stopped; its share of the \
                run budget was too small for the task, so raise --budget-secs"
            .to_string();
    }
    if outcome.cancelled {
        return "the worker was cancelled".to_string();
    }
    match (outcome.exit.code, outcome.exit.signal) {
        (Some(code), _) => format!("the worker exited with code {code}"),
        (_, Some(signal)) => format!("the worker was killed by signal {signal}"),
        _ => "the worker did not finish cleanly".to_string(),
    }
}
