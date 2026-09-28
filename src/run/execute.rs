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

use std::path::PathBuf;

use serde::Serialize;

use crate::run::record::{Run, RunError, Runs, Status};
use crate::triage::{Decision, Route};

/// The environment variable a worker finds its grant in.
///
/// Matched by the provider configuration's `env_http_headers`, which is what
/// turns it into `x-timon-grant` on the request.
pub const GRANT_ENV: &str = "TIMON_GRANT";

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
    /// The account-facing identity of the authority this ran under.
    pub grant_id: String,
    pub status: Status,
    /// The worker's own output, as it produced it.
    pub output: Option<String>,
    pub reported_tokens: Option<u64>,
    pub detail: Option<String>,
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
        // The planner route runs a lead and a graph, which is P3. Saying so is
        // better than quietly running one worker and calling it the planner.
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

    let outcome = run_worker(run, command, plan, &granted).await;

    // The run's authority ends with the run, on every path out of here.
    revoke(&plan.broker, &run.id);

    let (status, detail) = match &outcome {
        Ok(_) => (Status::Finished, None),
        Err(why) => (Status::Finished, Some(why.clone())),
    };
    runs.settle(&run.id, status, now, detail.as_deref())
        .map_err(ExecuteError::Store)?;

    Ok(Executed {
        run_id: run.id.clone(),
        route: decision.route,
        grant_id: granted.id,
        status,
        output: outcome.as_ref().ok().and_then(|o| o.output.clone()),
        reported_tokens: outcome.as_ref().ok().and_then(|o| o.tokens),
        detail,
    })
}

struct WorkerOutput {
    output: Option<String>,
    tokens: Option<u64>,
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
) -> Result<WorkerOutput, String> {
    use crate::worker::{WorkerLimits, WorkerSpec, run_worker as spawn};

    let (program, args) = command
        .split_first()
        .ok_or_else(|| "the worker command is empty".to_string())?;

    let spec = WorkerSpec {
        program: PathBuf::from(program),
        args: args.to_vec(),
        cwd: run.workspace.clone(),
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
        return "the worker passed its deadline and was stopped".to_string();
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
