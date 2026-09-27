// Adapted for Timon. Not derived from Prodex source.
//! Plan, delegate, integrate — driven by the host, decided by the lead.
//!
//! The lead cannot call a tool while it is sandboxed: on the pinned harness an
//! MCP tool call is refused unless approvals and the sandbox are both disabled,
//! and an unsandboxed lead is the very thing the design was built to avoid. So
//! the host sequences the phases instead, and the lead keeps the decisions.
//!
//! The host chooses *nothing* about the work. It does not invent a task, drop
//! one, reword one, or summarise a result. It runs what the lead asked for and
//! hands back exactly what came out, including the failures. Anything else would
//! quietly move planning authority from the lead into this file.
//!
//! One round only. A lead that could replan on seeing results would need loop
//! bounds, revision lineage and a way to supersede work in flight, which is the
//! host DAG the plan defers.

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::admission::{Ledger, RunLimits};
use crate::attempt::{
    AttemptReport, AttemptSpec, Role, UsageSource, fill_placeholders, run_attempt,
};
use crate::result::ResultStatus;
use crate::usage::{Accumulation, TokenCount};
use crate::worker::slots::{SlotError, SlotPool};
use crate::worker::{WorkerLimits, WorkerSpec};

/// The contract the lead's plan must satisfy.
pub const PLAN_SCHEMA: &str = r#"{
  "type": "object",
  "additionalProperties": false,
  "required": ["tasks", "notes"],
  "properties": {
    "tasks": {
      "type": "array",
      "items": {
        "type": "object",
        "additionalProperties": false,
        "required": ["label", "task"],
        "properties": {
          "label": { "type": "string" },
          "task": { "type": "string" }
        }
      }
    },
    "notes": { "type": "string" }
  }
}"#;

/// What the lead asked for.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Plan {
    pub tasks: Vec<PlannedTask>,
    /// The lead's own reasoning about the split, in its words.
    pub notes: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PlannedTask {
    pub label: String,
    pub task: String,
}

/// One delegated task and what came back.
#[derive(Clone, Debug, Serialize)]
pub struct Delegated {
    pub label: String,
    /// Exactly what the lead asked for, unedited.
    pub task: String,
    pub status: String,
    /// The worker's typed result, when it produced a usable one.
    pub result: Option<String>,
    pub reported_tokens: Option<u64>,
}

/// How a run is configured. None of it is the lead's to choose.
pub struct Phases {
    pub run_id: String,
    pub goal: String,
    pub lead_command: Vec<OsString>,
    pub worker_command: Vec<OsString>,
    pub output_root: PathBuf,
    pub lead_deadline: Duration,
    pub worker_deadline: Duration,
    /// Most tasks the lead may ask for. A plan asking for more is truncated and
    /// the truncation is reported, rather than silently obeyed or silently cut.
    pub max_tasks: usize,
    pub max_task_bytes: usize,
    pub max_output_bytes: u64,
    pub slots: Option<SlotPool>,
    pub ledger: Option<Ledger>,
    pub limits: RunLimits,
    pub usage_source: UsageSource,
    pub accumulation: Accumulation,
    /// Schema the final answer is held to, when the caller sets one.
    pub deliverable_schema: Option<PathBuf>,
}

/// What a whole run produced.
#[derive(Debug, Serialize)]
pub struct Outcome {
    pub run_id: String,
    pub plan: Option<Plan>,
    /// Tasks the lead asked for beyond the limit, which were not run.
    pub tasks_dropped_over_limit: usize,
    pub delegated: Vec<Delegated>,
    /// The lead's final answer, when it got that far.
    pub answer: Option<String>,
    pub lead_tokens: Option<u64>,
    pub worker_tokens: Option<u64>,
    /// Why the run ended early, when it did.
    pub stopped: Option<String>,
    pub basis: &'static str,
}

/// What the totals in an outcome are.
pub const USAGE_BASIS: &str = "Token figures are what each attempt reported, summed. An attempt \
that reported nothing contributes nothing, so a total is a lower bound rather than a measurement.";

/// Runs one goal through plan, delegate and integrate.
pub async fn run(phases: &Phases) -> anyhow::Result<Outcome> {
    let mut outcome = Outcome {
        run_id: phases.run_id.clone(),
        plan: None,
        tasks_dropped_over_limit: 0,
        delegated: Vec::new(),
        answer: None,
        lead_tokens: None,
        worker_tokens: None,
        stopped: None,
        basis: USAGE_BASIS,
    };
    let mut lead_total = Accumulated::default();
    let mut worker_total = Accumulated::default();

    // --- plan ---------------------------------------------------------------
    // The lead is admitted like any other attempt. It is usually the most
    // expensive part of a run -- a ceiling that covered only the workers would
    // leave the dominant cost unbounded, which is worse than having no ceiling
    // because it looks like one.
    if let Some(ledger) = &phases.ledger {
        match ledger.admit("lead-plan", &phases.limits) {
            Ok(Err(refusal)) => {
                outcome.stopped = Some(format!("the run could not even start planning: {refusal}"));
                return Ok(outcome);
            }
            Err(error) => {
                outcome.stopped = Some(format!("the run's allowance could not be read: {error}"));
                return Ok(outcome);
            }
            Ok(Ok(_)) => {}
        }
    }
    let plan_dir = phases.output_root.join("lead-plan");
    let plan_schema = plan_dir.join("plan-schema.json");
    create_private_dir(&plan_dir)?;
    std::fs::write(&plan_schema, PLAN_SCHEMA)?;
    let plan_report = attempt(
        phases,
        Role::Lead,
        "plan",
        &plan_dir,
        plan_prompt(&phases.goal, phases.max_tasks),
        &phases.lead_command,
        Some(plan_schema),
        phases.lead_deadline,
    )
    .await?;
    lead_total.add(plan_report.usage.total);
    if let Some(ledger) = &phases.ledger {
        let _ = ledger.settle("lead-plan", plan_report.usage.total.value());
    }

    let plan: Plan = match read_result(&plan_report, &plan_dir) {
        Some(body) => match serde_json::from_str(&body) {
            Ok(plan) => plan,
            Err(error) => {
                outcome.stopped = Some(format!(
                    "the lead's plan did not match its contract: {error}. Nothing was delegated."
                ));
                outcome.lead_tokens = lead_total.value();
                return Ok(outcome);
            }
        },
        None => {
            outcome.stopped =
                Some("the lead produced no usable plan, so nothing was delegated".to_string());
            outcome.lead_tokens = lead_total.value();
            return Ok(outcome);
        }
    };

    let mut tasks = plan.tasks.clone();
    if tasks.len() > phases.max_tasks {
        outcome.tasks_dropped_over_limit = tasks.len() - phases.max_tasks;
        tasks.truncate(phases.max_tasks);
    }
    outcome.plan = Some(plan);

    // --- delegate -----------------------------------------------------------
    for (index, task) in tasks.iter().enumerate() {
        let attempt_id = format!("w{}-{}", index + 1, sanitise(&task.label));

        if let Some(ledger) = &phases.ledger {
            match ledger.admit(&attempt_id, &phases.limits) {
                Ok(Err(refusal)) => {
                    // Stop delegating, but still integrate what came back: the
                    // lead can say what is missing, which is more use than
                    // abandoning the run.
                    outcome.stopped = Some(format!("delegation stopped: {refusal}"));
                    break;
                }
                Err(error) => {
                    outcome.stopped =
                        Some(format!("the run's allowance could not be read: {error}"));
                    break;
                }
                Ok(Ok(_)) => {}
            }
        }

        let _lease = match &phases.slots {
            Some(pool) => match pool.try_acquire() {
                Ok(lease) => Some(lease),
                Err(SlotError::Full) => {
                    outcome.stopped = Some(
                        "every worker slot on this host is busy; delegation stopped".to_string(),
                    );
                    if let Some(ledger) = &phases.ledger {
                        let _ = ledger.settle(&attempt_id, Some(0));
                    }
                    break;
                }
                Err(error) => {
                    outcome.stopped = Some(format!("a worker slot could not be taken: {error}"));
                    break;
                }
            },
            None => None,
        };

        let dir = phases.output_root.join(format!("worker-{attempt_id}"));
        create_private_dir(&dir)?;
        let report = attempt(
            phases,
            Role::Worker,
            &attempt_id,
            &dir,
            task.task.clone(),
            &phases.worker_command,
            phases.deliverable_schema.clone(),
            phases.worker_deadline,
        )
        .await?;
        worker_total.add(report.usage.total);
        if let Some(ledger) = &phases.ledger {
            let _ = ledger.settle(&attempt_id, report.usage.total.value());
        }

        outcome.delegated.push(Delegated {
            label: task.label.clone(),
            task: task.task.clone(),
            status: describe_status(&report),
            result: read_result(&report, &dir),
            reported_tokens: report.usage.total.value(),
        });
    }

    // --- integrate ----------------------------------------------------------
    // Runs even when delegation stopped early or every worker failed. The lead
    // saying what is missing is worth more than the host deciding the run failed.
    // Integration is admitted too, but a refusal here does not throw away the
    // workers' results: the run reports what it has and says why it stopped.
    let mut integrate_refused = None;
    if let Some(ledger) = &phases.ledger {
        match ledger.admit("lead-integrate", &phases.limits) {
            Ok(Err(refusal)) => integrate_refused = Some(format!("{refusal}")),
            Err(error) => integrate_refused = Some(format!("{error}")),
            Ok(Ok(_)) => {}
        }
    }
    if let Some(reason) = integrate_refused {
        outcome.stopped = Some(match outcome.stopped.take() {
            Some(earlier) => {
                format!("{earlier}; the lead could not be called to integrate either: {reason}")
            }
            None => format!(
                "the workers finished but the lead could not be called to integrate: {reason}. Their results are below, uncombined."
            ),
        });
        outcome.lead_tokens = lead_total.value();
        outcome.worker_tokens = worker_total.value();
        return Ok(outcome);
    }

    let integrate_dir = phases.output_root.join("lead-integrate");
    create_private_dir(&integrate_dir)?;
    let integrate_report = attempt(
        phases,
        Role::Lead,
        "integrate",
        &integrate_dir,
        integrate_prompt(&phases.goal, &outcome.delegated, outcome.stopped.as_deref()),
        &phases.lead_command,
        phases.deliverable_schema.clone(),
        phases.lead_deadline,
    )
    .await?;
    lead_total.add(integrate_report.usage.total);
    if let Some(ledger) = &phases.ledger {
        let _ = ledger.settle("lead-integrate", integrate_report.usage.total.value());
    }
    outcome.answer = read_result(&integrate_report, &integrate_dir);
    if outcome.answer.is_none() {
        outcome.stopped = Some(match outcome.stopped.take() {
            Some(earlier) => format!("{earlier}; the lead then produced no usable final answer"),
            None => "the lead produced no usable final answer".to_string(),
        });
    }

    outcome.lead_tokens = lead_total.value();
    outcome.worker_tokens = worker_total.value();
    Ok(outcome)
}

/// Runs one phase as an attempt.
#[allow(clippy::too_many_arguments)]
async fn attempt(
    phases: &Phases,
    role: Role,
    attempt_id: &str,
    dir: &std::path::Path,
    task: String,
    command: &[OsString],
    schema: Option<PathBuf>,
    deadline: Duration,
) -> anyhow::Result<AttemptReport> {
    let result_file = dir.join("result.json");
    let command = fill_placeholders(command, dir, Some(&result_file), schema.as_deref());
    let spec = AttemptSpec {
        role,
        run_id: phases.run_id.clone(),
        attempt_id: attempt_id.to_string(),
        client_event_id: None,
        worker: WorkerSpec {
            program: PathBuf::from(&command[0]),
            args: command[1..].to_vec(),
            task,
            cwd: None,
            env_set: vec![
                (OsString::from("TIMON_ATTEMPT_DIR"), OsString::from(dir)),
                (
                    OsString::from("TIMON_RESULT_FILE"),
                    OsString::from(&result_file),
                ),
            ],
            env_remove: Vec::new(),
            output_dir: dir.to_path_buf(),
            limits: WorkerLimits {
                max_task_bytes: phases.max_task_bytes,
                max_output_bytes: phases.max_output_bytes,
                ..WorkerLimits::with_deadline(deadline)
            },
        },
        result_file: Some(result_file),
        result_schema: schema,
        usage_source: phases.usage_source.clone(),
        accumulation: phases.accumulation,
    };
    run_attempt(&spec, std::future::pending()).await
}

/// The lead's planning instructions.
///
/// The constraint that matters is stated first: a worker sees only its own task.
/// Every other rule here follows from that, and a lead that misses it writes
/// tasks referring to context the worker will never have.
fn plan_prompt(goal: &str, max_tasks: usize) -> String {
    format!(
        "You are the lead on this goal:\n\n{goal}\n\n\
Decide how to approach it, then split out the parts a cheaper model could do \
independently while you keep the judgement.\n\n\
A worker starts fresh. It sees only the text you write for it: not this \
instruction, not the goal, not the other workers, and not anything you have \
worked out so far. So each task must stand completely on its own -- say what to \
find or produce, include any input it needs inline, and state exactly what its \
answer should look like.\n\n\
Delegate only work that can be described fully in advance and checked when it \
returns. Keep the comparison, the weighing up and the final answer for yourself: \
you will be called again with every worker's result to produce it.\n\n\
Ask for at most {max_tasks} task(s), and fewer where fewer will do -- each one \
costs tokens from a fixed allowance. If the goal needs no delegation at all, \
return an empty list and say why in notes.\n\n\
Reply with JSON only, in exactly this shape:\n\
{{\"tasks\":[{{\"label\":\"short-name\",\"task\":\"the complete task\"}}],\"notes\":\"why you split it this way\"}}"
    )
}

/// The lead's integration instructions.
fn integrate_prompt(goal: &str, delegated: &[Delegated], stopped: Option<&str>) -> String {
    let mut out = format!(
        "You are the lead. This was the goal:\n\n{goal}\n\n\
You asked for the tasks below and this is what came back, unedited.\n\n"
    );
    if delegated.is_empty() {
        out.push_str("No worker returned anything.\n\n");
    }
    for item in delegated {
        out.push_str(&format!("--- worker: {} ---\n", item.label));
        out.push_str(&format!("you asked: {}\n", item.task));
        out.push_str(&format!("status: {}\n", item.status));
        match &item.result {
            Some(body) => out.push_str(&format!("returned:\n{body}\n\n")),
            None => out.push_str("returned: nothing usable\n\n"),
        }
    }
    if let Some(reason) = stopped {
        out.push_str(&format!("The run stopped early: {reason}\n\n"));
    }
    out.push_str(
        "Now answer the goal. Use only what the workers returned and what you can \
reason from it. Where a worker failed or returned nothing usable, say what is \
missing and how that limits the answer -- do not fill the gap with something you \
have not been given.",
    );
    out
}

/// Sums reported token counts, keeping track of whether any were missing.
#[derive(Default)]
struct Accumulated {
    total: u64,
    saw_any: bool,
}

impl Accumulated {
    fn add(&mut self, count: TokenCount) {
        if let Some(value) = count.value() {
            self.total = self.total.saturating_add(value);
            self.saw_any = true;
        }
    }

    /// `None` when nothing was ever reported, so a caller cannot read an absence
    /// of reports as a genuine zero.
    fn value(&self) -> Option<u64> {
        self.saw_any.then_some(self.total)
    }
}

fn read_result(report: &AttemptReport, dir: &std::path::Path) -> Option<String> {
    if !matches!(report.result, ResultStatus::Parsed { .. }) {
        return None;
    }
    std::fs::read_to_string(dir.join("result.json")).ok()
}

fn describe_status(report: &AttemptReport) -> String {
    if report.process.timed_out {
        return "stopped at its deadline; it spent whatever tokens it used first".to_string();
    }
    if report.process.cancelled {
        return "cancelled".to_string();
    }
    if !report.process.succeeded() {
        return "failed".to_string();
    }
    match report.result {
        ResultStatus::Parsed { .. } => "returned a result matching its contract".to_string(),
        ref other => format!(
            "ran, but produced nothing usable: {}",
            serde_json::to_string(other).unwrap_or_default()
        ),
    }
}

fn create_private_dir(path: &std::path::Path) -> std::io::Result<()> {
    if path.exists() {
        return Ok(());
    }
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}

fn sanitise(label: &str) -> String {
    let cleaned: String = label
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(40)
        .collect();
    if cleaned.is_empty() {
        "task".to_string()
    } else {
        cleaned
    }
}
