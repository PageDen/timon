// Adapted for Timon. Not derived from Prodex source.
//! The planner route, joined up.
//!
//! P1 recorded the run, P2 chose the route, P3 said what a valid graph is and
//! what a dependency carries, P4 ran one and isolated the workers. Each worked
//! on its own; nothing connected them, and the planner route reported that it
//! was not built. This is the connection.
//!
//! The order is the whole design:
//!
//! 1. **The planner is asked for a graph**, not for an answer. It names tasks
//!    and what each depends on; it does not run anything.
//! 2. **The host validates it and refuses a bad one**, with every fault at
//!    once. It does not repair a plan — repairing means deciding what the
//!    planner meant, and the host does not author work.
//! 3. **Each task runs in its own worktree**, from inputs the host assembled,
//!    under a grant the broker issued.
//! 4. **The host merges mechanically** onto a result branch and hands it over.
//!
//! What it deliberately does not do is judge the result. That is P5. A result
//! branch out of here is a merge, not a recommendation, and the report says so
//! rather than letting a developer read approval into a clean exit.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use serde::Serialize;

use crate::dag::{Limits, Plan, Validated, validate};
use crate::dag_run::{Bounds, GraphReport, Runner, run_graph};
use crate::run::record::Run;
use crate::worktree::Workspace;

/// Why a planned run could not be carried out.
#[derive(Debug)]
pub enum PipelineError {
    /// The planner did not answer with a graph.
    NotAPlan {
        detail: String,
        saw: String,
    },
    /// The graph was refused. Every fault, so one round trip fixes all of them.
    InvalidPlan {
        faults: Vec<String>,
    },
    /// No repository to work in, which a writing graph needs.
    NoWorkspace,
    Git(String),
}

impl std::fmt::Display for PipelineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PipelineError::NotAPlan { detail, saw } => write!(
                f,
                "the planner did not return a task graph ({detail}). It said: {saw}"
            ),
            PipelineError::InvalidPlan { faults } => {
                write!(f, "the plan was refused:\n  - {}", faults.join("\n  - "))
            }
            PipelineError::NoWorkspace => write!(
                f,
                "this run has no repository, so there is nowhere to put a worktree"
            ),
            PipelineError::Git(detail) => write!(f, "{detail}"),
        }
    }
}

impl std::error::Error for PipelineError {}

/// What the planner route produced.
#[derive(Debug, Serialize)]
pub struct PipelineReport {
    pub run_id: String,
    /// The planner's own reasoning about the split, in its words.
    pub notes: String,
    pub graph: GraphReport,
    /// The branch to review, when integration got that far.
    pub result_branch: Option<String>,
    /// The criteria of the tasks that finished, gathered while the graph was
    /// still to hand. A task that never ran has not failed its criteria.
    pub criteria_of_finished: Option<Vec<crate::acceptance::Criterion>>,
    /// What the verifier made of it, when one ran.
    pub verdict: Option<crate::verify::Verdict>,
    /// Said plainly when nothing judged the result.
    pub caveat: Option<&'static str>,
}

/// What a planner route needs from the host.
pub struct Pipeline<'a, R: Runner + Sync> {
    pub runner: &'a R,
    pub workspace: Option<Workspace>,
    pub limits: Limits,
    pub bounds: Bounds,
    pub cancel: Arc<AtomicBool>,
}

/// Asks the planner for a graph.
///
/// The prompt says what a dependency means here, because a planner that thinks
/// `depends_on` is only ordering will write graphs whose dependants cannot see
/// what they depend on — which is the mistake the host spent P3 fixing.
pub fn plan_prompt(goal: &str, limits: &Limits) -> String {
    format!(
        "You are planning how to carry out this goal:\n\n{goal}\n\n\
Split it into tasks that can be worked on separately. Each task is given to a \
worker that starts fresh: it sees only the text you write for it, not this \
instruction, not the goal, and not the other tasks. So each task must stand on \
its own.\n\n\
When one task needs another's output, put that task's `label` in `depends_on` — \
the label exactly as you wrote it, not a description of what it produces. \
`\"depends_on\":[\"write-notes\"]`, never \
`\"depends_on\":[\"the completed notes file\"]`; a plan naming anything other than a \
label is refused and has to be written again. The dependency is not just \
ordering: the host gives a dependent task what its dependencies actually \
produced, so there is no need to describe it.\n\n\
Mark a task `\"access\": \"write\"` when it changes files, and `\"read\"` when it \
only reads. Writing tasks that touch the same code should be one task, not \
several — separate workers editing the same files conflict, and the host will \
not choose between them.\n\n\
Give each task an `acceptance` list: what must be true of the files afterwards \
for it to have been done. These are checked mechanically, so they must be \
claims about files rather than descriptions of quality — \
`{{\"kind\":\"file_exists\",\"path\":\"NOTES.md\"}}`, \
`{{\"kind\":\"file_contains\",\"path\":\"README.md\",\"text\":\"NOTES.md\"}}`, \
`{{\"kind\":\"file_absent\",\"path\":\"old.rs\"}}`, or \
`{{\"kind\":\"file_omits\",\"path\":\"lib.rs\",\"text\":\"deprecated_fn\"}}`. Paths are \
relative to the repository.\n\n\
A criterion has to do two things at once, and both are easy to lose.\n\n\
It must **fail if the task was not done**. \
`{{\"kind\":\"file_contains\",\"path\":\"TESTING.md\",\"text\":\"test\"}}` is useless: a \
file called TESTING.md contains the word \"test\" whatever is in it. Check \
something only the finished work would contain — a command that has to be run, \
the name of the thing being documented, the identifier that was added.\n\n\
It must **not fail work that is correct**. A criterion stricter than the task is \
one the work can fail while being right. If the task says to link to a file with \
clear link text, check the filename appears — not that the link reads exactly \
\"[FILE.md](FILE.md)\", which would fail the clear link text you asked for.\n\n\
A task with no checkable criteria gets none, and its result is reported as \
unverified rather than as done. That is better than a criterion that is wrong.\n\n\
At most {} tasks, at most {} deep. Fewer where fewer will do.\n\n\
Reply with JSON only:\n\
{{\"tasks\":[{{\"label\":\"short-name\",\"task\":\"the complete task\",\"depends_on\":[],\"access\":\"read\",\"acceptance\":[{{\"kind\":\"file_exists\",\"path\":\"x.md\"}}]}}],\"notes\":\"why you split it this way\"}}",
        limits.max_tasks, limits.max_depth
    )
}

/// Reads a plan out of whatever the planner said.
///
/// Tolerant of a model that wrapped its JSON in prose or a fence, because that
/// is a formatting slip rather than a refusal to plan, and a round trip to
/// correct it costs a model call.
pub fn parse_plan(answer: &str) -> Result<Plan, PipelineError> {
    let candidate = extract_json(answer).ok_or_else(|| PipelineError::NotAPlan {
        detail: "no JSON object in the answer".to_string(),
        saw: first_line(answer),
    })?;
    serde_json::from_str(&candidate).map_err(|error| PipelineError::NotAPlan {
        detail: error.to_string(),
        saw: first_line(&candidate),
    })
}

fn extract_json(text: &str) -> Option<String> {
    let start = text.find('{')?;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, ch) in text[start..].char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' if in_string => escaped = true,
            '"' => in_string = !in_string,
            '{' if !in_string => depth += 1,
            '}' if !in_string => {
                depth -= 1;
                if depth == 0 {
                    return Some(text[start..start + offset + 1].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

fn first_line(text: &str) -> String {
    text.lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
        .chars()
        .take(200)
        .collect()
}

/// Validates a plan, turning faults into something a planner can act on.
pub fn accept(plan: &Plan, limits: &Limits) -> Result<Validated, PipelineError> {
    validate(plan, limits).map_err(|faults| PipelineError::InvalidPlan {
        faults: faults.iter().map(|fault| fault.to_string()).collect(),
    })
}

/// Validates a plan and checks it can finish inside the budget.
///
/// Both before anything runs. The host knows the depth and the per-worker
/// ceiling, so it can say in advance whether a graph fits — and a plan refused
/// in advance costs nothing, while one discovered not to fit at minute four
/// costs the whole run.
pub fn accept_within(
    plan: &Plan,
    limits: &Limits,
    budget: &crate::budget::Budget,
) -> Result<Validated, PipelineError> {
    let graph = accept(plan, limits)?;
    budget
        .fits(graph.depth())
        .map_err(|why| PipelineError::InvalidPlan { faults: vec![why] })?;
    Ok(graph)
}

/// Runs an accepted graph and merges what it produced.
///
/// Integration happens even when tasks failed: the branches that did succeed are
/// still work somebody may want, and withholding them because a sibling failed
/// decides for the developer. What does not happen is any claim about quality.
pub fn carry_out<R: Runner + Sync>(
    run: &Run,
    graph: &Validated,
    pipeline: &mut Pipeline<'_, R>,
) -> PipelineReport {
    let report = run_graph(
        graph,
        pipeline.runner,
        &pipeline.bounds,
        Arc::clone(&pipeline.cancel),
    );

    let result_branch = match pipeline.workspace.as_mut() {
        Some(workspace) => {
            let finished: Vec<String> = report
                .tasks
                .iter()
                .filter(|task| task.outcome.finished())
                .map(|task| task.label.clone())
                .collect();
            if finished.is_empty() {
                None
            } else {
                workspace.integrate(&finished).ok()
            }
        }
        None => None,
    };

    let criteria_of_finished = report
        .tasks
        .iter()
        .filter(|task| task.outcome.finished())
        .filter_map(|task| graph.task(&task.label))
        .flat_map(|task| task.acceptance.clone())
        .collect();

    PipelineReport {
        run_id: run.id.clone(),
        notes: graph.notes.clone(),
        graph: report,
        result_branch,
        criteria_of_finished: Some(criteria_of_finished),
        verdict: None,
        caveat: Some(NOT_JUDGED),
    }
}

/// Said when nothing judged a result, so a clean exit is not read as approval.
pub const NOT_JUDGED: &str = "Nothing judged this result. The branch is a merge of what the workers produced, not a recommendation.";

/// Judges a finished run and attaches the verdict.
///
/// Separate from carrying it out, because the two fail for different reasons and
/// a verifier that cannot run should not make a completed run look failed.
pub fn judge<C: crate::verify::Checks>(
    report: &mut PipelineReport,
    tested: Option<String>,
    criteria: Vec<crate::acceptance::Criterion>,
    checks: &C,
) {
    let subject = crate::verify::Subject {
        execution: &report.graph,
        branch: report.result_branch.as_deref(),
        tested,
        criteria,
    };
    report.verdict = Some(crate::verify::verify(&subject, checks));
    report.caveat = None;
}
