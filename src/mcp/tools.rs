// Adapted for Timon. Not derived from Prodex source.
//! The delegation tool itself.

use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::admission::{Ledger, RunLimits};
use crate::attempt::{AttemptSpec, Role, UsageSource, fill_placeholders, run_attempt};
use crate::usage::Accumulation;
use crate::worker::slots::{SlotError, SlotPool};
use crate::worker::{WorkerLimits, WorkerSpec};

/// How the server was told to run workers.
///
/// The lead does not choose any of this. A model that could pick its own worker
/// command, sandbox mode or budget would not be bounded by any of them, so every
/// one is fixed by whoever started the server.
#[derive(Clone, Debug)]
pub struct WorkerPolicy {
    /// Program and arguments for a worker. The task is delivered on stdin and
    /// never appears here. Placeholders above are filled in per attempt.
    pub command: Vec<std::ffi::OsString>,
    pub run_id: String,
    pub output_root: PathBuf,
    pub deadline: Duration,
    pub max_task_bytes: usize,
    pub max_output_bytes: u64,
    pub slots: Option<SlotPool>,
    pub ledger: Option<Ledger>,
    pub limits: RunLimits,
    pub usage_source: UsageSource,
    pub accumulation: Accumulation,
    /// Result file the worker is told to write, relative to its attempt
    /// directory, and the schema it is held to.
    pub result_file: Option<String>,
    pub result_schema: Option<PathBuf>,
}

/// What a lead may ask for.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DelegateRequest {
    /// The bounded task, in full. A worker starts fresh and sees nothing else.
    pub task: String,
    /// A label for this piece of work, used in the accounting.
    #[serde(default)]
    pub label: Option<String>,
}

/// The delegation tool, as advertised to a lead.
pub fn describe() -> Value {
    json!({
        "name": "delegate_worker",
        "description": "\
    Run one bounded task on a cheaper model and return its typed result and token \
    usage. The worker starts with no memory of this conversation, so the task must \
    be self-contained: state the goal, the inputs, and exactly what the deliverable \
    should look like. Use this for work that can be described completely and checked \
    on its return -- gathering evidence, extracting structure, summarising a source. \
    Do the planning, the integration and the judgement yourself; a worker cannot see \
    the wider problem and cannot delegate further. Every call spends tokens against \
    this run's allowance and may be refused when that allowance is spent.",
        "inputSchema": {
            "type": "object",
            "additionalProperties": false,
            "required": ["task"],
            "properties": {
                "task": {
                    "type": "string",
                    "description": "The complete, self-contained task for the worker."
                },
                "label": {
                    "type": "string",
                    "description": "Short name for this piece of work, for the accounting."
                }
            }
        }
    })
}

/// Runs one delegated task.
///
/// Returns the text a lead sees and whether it should be read as a failure.
pub async fn delegate(
    policy: &WorkerPolicy,
    attempt_number: u64,
    request: DelegateRequest,
) -> (String, bool) {
    let attempt_id = match &request.label {
        Some(label) => format!("{attempt_number}-{}", sanitise(label)),
        None => attempt_number.to_string(),
    };

    if request.task.trim().is_empty() {
        return ("The task was empty. Nothing was run.".to_string(), true);
    }
    if request.task.len() > policy.max_task_bytes {
        return (
            format!(
                "The task is {} bytes, over the {} byte limit. Send less, or split it.",
                request.task.len(),
                policy.max_task_bytes
            ),
            true,
        );
    }

    // Admission before anything is started, so a spent run cannot be talked into
    // one more worker.
    if let Some(ledger) = &policy.ledger {
        match ledger.admit(&attempt_id, &policy.limits) {
            Ok(Err(refusal)) => {
                return (
                    format!(
                        "Not delegated: {refusal}. Finish with what you already have, and say \
what is missing."
                    ),
                    true,
                );
            }
            Err(error) => {
                return (
                    format!("The run's allowance could not be read: {error}"),
                    true,
                );
            }
            Ok(Ok(_)) => {}
        }
    }

    let _lease = match &policy.slots {
        Some(pool) => match pool.try_acquire() {
            Ok(lease) => Some(lease),
            Err(SlotError::Full) => {
                return (
                    "Every worker slot on this host is busy. Try again shortly, or do this \
piece yourself."
                        .to_string(),
                    true,
                );
            }
            Err(error @ SlotError::YoursFull { .. }) => {
                return (
                    format!(
                        "{error}. Wait for one of your own workers to finish, or do this \
piece yourself."
                    ),
                    true,
                );
            }
            Err(error) => return (format!("A worker slot could not be taken: {error}"), true),
        },
        None => None,
    };

    let attempt_dir = policy.output_root.join(format!("attempt-{attempt_id}"));
    if let Err(error) = create_private_dir(&attempt_dir) {
        return (
            format!("The worker's output directory could not be made: {error}"),
            true,
        );
    }
    let result_file = policy
        .result_file
        .as_ref()
        .map(|name| attempt_dir.join(name));

    // The command is only now complete: it names paths that did not exist until
    // this attempt's directory was made.
    let command = fill_placeholders(
        &policy.command,
        &attempt_dir,
        result_file.as_deref(),
        policy.result_schema.as_deref(),
    );

    let spec = AttemptSpec {
        role: Role::Worker,
        run_id: policy.run_id.clone(),
        attempt_id: attempt_id.clone(),
        client_event_id: None,
        worker: WorkerSpec {
            program: PathBuf::from(&command[0]),
            args: command[1..].to_vec(),
            task: request.task,
            cwd: None,
            // Also offered as environment variables, for a worker that would
            // rather read them than be given them as arguments.
            env_set: {
                let mut env = vec![(
                    std::ffi::OsString::from("TIMON_ATTEMPT_DIR"),
                    std::ffi::OsString::from(&attempt_dir),
                )];
                if let Some(path) = &result_file {
                    env.push((
                        std::ffi::OsString::from("TIMON_RESULT_FILE"),
                        std::ffi::OsString::from(path),
                    ));
                }
                env
            },
            env_remove: Vec::new(),
            output_dir: attempt_dir.clone(),
            // Built from the constructor so the drain and reap timeouts stay
            // whatever the supervisor considers sound, rather than being restated
            // here and drifting.
            limits: WorkerLimits {
                max_task_bytes: policy.max_task_bytes,
                max_output_bytes: policy.max_output_bytes,
                ..WorkerLimits::with_deadline(policy.deadline)
            },
        },
        result_file: result_file.clone(),
        result_schema: policy.result_schema.clone(),
        usage_source: policy.usage_source.clone(),
        accumulation: policy.accumulation,
    };

    let report = match run_attempt(&spec, std::future::pending()).await {
        Ok(report) => report,
        Err(error) => {
            if let Some(ledger) = &policy.ledger {
                let _ = ledger.settle(&attempt_id, None);
            }
            return (format!("The worker could not be started: {error:#}"), true);
        }
    };

    // Settled with whatever was reported, including nothing. Tokens were spent
    // whether or not the work was any good.
    if let Some(ledger) = &policy.ledger {
        let _ = ledger.settle(&attempt_id, report.usage.total.value());
    }

    (render(&report, result_file.as_deref()), !report.succeeded())
}

/// What the lead is told about a finished worker.
///
/// Deliberately explicit about usage being reported rather than measured, and
/// about a timeout having spent tokens anyway: a lead that believes a killed
/// worker was free will keep retrying it.
fn render(report: &crate::attempt::AttemptReport, result_file: Option<&std::path::Path>) -> String {
    let mut out = String::new();
    if report.process.timed_out {
        out.push_str(
            "The worker hit its deadline and was stopped. It may have done part of the work, \
and it spent whatever tokens it used before stopping.\n\n",
        );
    } else if !report.process.succeeded() {
        out.push_str("The worker failed.\n\n");
    }

    match report.result {
        crate::result::ResultStatus::Parsed { .. } => {
            match result_file.and_then(|path| std::fs::read_to_string(path).ok()) {
                Some(body) => {
                    out.push_str("Result:\n");
                    out.push_str(&body);
                    out.push('\n');
                }
                None => out.push_str("The worker reported a result that could not be read back.\n"),
            }
        }
        crate::result::ResultStatus::NotRequested => {
            out.push_str("No typed result was asked for.\n");
        }
        ref other => {
            out.push_str(&format!(
                "No usable result: {}. Treat this attempt as having produced nothing.\n",
                serde_json::to_string(other).unwrap_or_default()
            ));
        }
    }

    out.push_str(&format!(
        "\nReported usage: {}. This is what the worker reported, not a measurement.\n",
        match report.usage.total.value() {
            Some(total) => format!("{total} tokens"),
            None => "unknown, which is not the same as none".to_string(),
        }
    ));
    out
}

fn create_private_dir(path: &std::path::Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    if path.exists() {
        return Ok(());
    }
    builder.create(path)
}

/// Keeps a label usable as part of a path and an attempt id.
fn sanitise(label: &str) -> String {
    label
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(40)
        .collect()
}

/// Re-exported so the server can advertise every tool in one place.
pub fn all() -> Vec<Value> {
    vec![describe()]
}
