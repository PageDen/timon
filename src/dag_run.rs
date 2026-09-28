// Adapted for Timon. Not derived from Prodex source.
//! Running a validated graph.
//!
//! The scheduler's whole job is to do less than it looks like it should. It
//! starts tasks whose dependencies are done, stops at the limits it was given,
//! and reports. It does not decide what a task is, does not repair a plan, and
//! does not resolve a conflict — each of those would be the host authoring work,
//! which is the invariant the plan keeps.
//!
//! Three behaviours are worth stating because the obvious implementation gets
//! them wrong.
//!
//! **A failed task blocks its dependants, and they are reported as not run.**
//! Not as failed. They were never attempted, and a report that cannot tell those
//! apart sends somebody debugging work that never started.
//!
//! **Cancellation reaches the workers**, not just the scheduler. Stopping the
//! loop while children keep talking to a provider is not cancelling, it is
//! losing track. This closes the gap P2's executor left open and TODO.md
//! recorded.
//!
//! **The deadline stops new work and lets running work finish or be killed by
//! its own limit.** Starting a task at the deadline so it can be killed a second
//! later spends tokens for nothing.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::Serialize;

use crate::dag::{Task, Validated};
use crate::dag_inputs::{Artifact, Input, InputError, inputs_for};

/// How a task ended.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Outcome {
    /// Produced an artifact its dependants can consume.
    Done { artifact: Artifact },
    /// Attempted and failed, with the reason as the worker gave it.
    Failed { detail: String },
    /// Never attempted, because something it depends on did not finish.
    ///
    /// Distinct from failure on purpose: nobody should debug a task that never
    /// started.
    NotRun { blocked_by: Vec<String> },
    /// Cancelled, or not started because the run was.
    Cancelled,
    /// Not started because the run's deadline had passed.
    Skipped,
}

impl Outcome {
    pub fn finished(&self) -> bool {
        matches!(self, Outcome::Done { .. })
    }
}

/// One finished task, as its thread hands it back.
///
/// Named rather than left as a tuple: five positional fields at a join point is
/// where a swapped pair of timestamps hides.
struct Ran {
    label: String,
    result: Result<Artifact, String>,
    input: Input,
    started_at: i64,
    ended_at: i64,
}

/// What one task did.
#[derive(Clone, Debug, Serialize)]
pub struct TaskReport {
    pub label: String,
    pub outcome: Outcome,
    /// What the task was given to work from, so a reader can reproduce it.
    pub input: Option<Input>,
    pub started_at: Option<i64>,
    pub ended_at: Option<i64>,
}

/// What a whole graph did.
#[derive(Clone, Debug, Serialize)]
pub struct GraphReport {
    pub tasks: Vec<TaskReport>,
    /// True when every task produced something.
    pub complete: bool,
    /// Tasks that were never attempted, and why. Lifted out of the per-task
    /// reports because it is the first thing anybody asks.
    pub not_run: Vec<String>,
}

impl GraphReport {
    pub fn artifact(&self, label: &str) -> Option<&Artifact> {
        self.tasks.iter().find_map(|report| match &report.outcome {
            Outcome::Done { artifact } if report.label == label => Some(artifact),
            _ => None,
        })
    }
}

/// Limits the scheduler is held to. The planner chooses none of them.
#[derive(Clone, Copy, Debug)]
pub struct Bounds {
    /// Tasks that may run at once.
    pub concurrency: usize,
    /// Unix seconds after which no further task is started.
    pub deadline: Option<i64>,
    /// Whether this host has qualified its write sandbox.
    ///
    /// Not advisory. The plan says the sandbox is qualified *before* any worker
    /// writes, and a flag somebody can forget to check is not a gate — so the
    /// scheduler asks here, every time, rather than trusting that whoever built
    /// the plan remembered.
    pub writing_permitted: bool,
}

impl Default for Bounds {
    fn default() -> Self {
        Bounds {
            concurrency: 3,
            deadline: None,
            // Shut by default. A host that has not qualified has not qualified.
            writing_permitted: false,
        }
    }
}

/// What the scheduler needs from the host to run one task.
///
/// A trait so the graph logic can be tested without a model, a worktree or a
/// provider. The scheduler's decisions are what this module is for; running a
/// process is somebody else's job.
pub trait Runner {
    /// Runs one task with the input assembled for it.
    ///
    /// Returns the artifact it produced, or why it failed. Must stop promptly
    /// when `cancel` becomes true.
    fn run(&self, task: &Task, input: &Input, cancel: &AtomicBool) -> Result<Artifact, String>;

    /// Prepares a commit containing the named tasks' changes, for a writing
    /// task. Returns the reason on conflict, which becomes a verifier finding.
    fn prepare(&self, _merged: &[String]) -> Result<String, String> {
        Err("this host cannot prepare commits, so writing tasks cannot run".to_string())
    }

    /// Wall-clock now, as unix seconds. Injected so tests are not slow.
    fn now(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_secs() as i64)
            .unwrap_or_default()
    }
}

/// Runs a validated graph within its bounds.
///
/// Tasks run in threads, up to `concurrency` at once. Threads rather than async
/// because a task is a child process and the scheduler spends its life waiting;
/// the complexity of an async runtime would buy nothing here.
pub fn run_graph<R: Runner + Sync>(
    plan: &Validated,
    runner: &R,
    bounds: &Bounds,
    cancel: Arc<AtomicBool>,
) -> GraphReport {
    let mut done: BTreeSet<String> = BTreeSet::new();
    let mut produced: BTreeMap<String, Artifact> = BTreeMap::new();
    let mut reports: BTreeMap<String, TaskReport> = BTreeMap::new();

    loop {
        // Anything whose dependencies are satisfied and which has not been
        // decided yet.
        let ready: Vec<&Task> = plan
            .ready(&done)
            .into_iter()
            .filter(|task| !reports.contains_key(&task.label))
            .collect();

        if ready.is_empty() {
            break;
        }

        let batch: Vec<&Task> = ready.into_iter().take(bounds.concurrency).collect();

        // Each of these is decided before a worker starts, because starting one
        // to stop it immediately spends tokens for nothing.
        let mut to_run = Vec::new();
        for task in batch {
            if cancel.load(Ordering::Relaxed) {
                reports.insert(
                    task.label.clone(),
                    TaskReport {
                        label: task.label.clone(),
                        outcome: Outcome::Cancelled,
                        input: None,
                        started_at: None,
                        ended_at: None,
                    },
                );
                continue;
            }
            if let Some(deadline) = bounds.deadline
                && runner.now() >= deadline
            {
                reports.insert(
                    task.label.clone(),
                    TaskReport {
                        label: task.label.clone(),
                        outcome: Outcome::Skipped,
                        input: None,
                        started_at: None,
                        ended_at: None,
                    },
                );
                continue;
            }

            if task.access == crate::dag::Access::Write && !bounds.writing_permitted {
                reports.insert(
                    task.label.clone(),
                    TaskReport {
                        label: task.label.clone(),
                        outcome: Outcome::Failed {
                            detail: "this host has not qualified its write sandbox, so \
tasks that change code are not run. `timon qualify write-sandbox` says what is \
outstanding"
                                .to_string(),
                        },
                        input: None,
                        started_at: None,
                        ended_at: None,
                    },
                );
                continue;
            }

            match inputs_for(task, None, &produced, |merged| runner.prepare(merged)) {
                Ok(input) => to_run.push((task, input)),
                Err(error) => {
                    // A conflict or an out-of-order start. Reported as a failure
                    // of this task, with the host's reason, and never resolved
                    // by guessing.
                    reports.insert(
                        task.label.clone(),
                        TaskReport {
                            label: task.label.clone(),
                            outcome: Outcome::Failed {
                                detail: describe(&error),
                            },
                            input: None,
                            started_at: None,
                            ended_at: None,
                        },
                    );
                }
            }
        }

        if to_run.is_empty() {
            // Everything in this round was decided without running. Mark what is
            // still undecided and stop, rather than spinning.
            if reports.len() < plan.tasks().len() {
                continue;
            }
            break;
        }

        let results: Vec<Ran> = std::thread::scope(|scope| {
            let handles: Vec<_> = to_run
                .into_iter()
                .map(|(task, input)| {
                    let cancel = Arc::clone(&cancel);
                    scope.spawn(move || {
                        let started_at = runner.now();
                        let result = runner.run(task, &input, &cancel);
                        Ran {
                            label: task.label.clone(),
                            result,
                            input,
                            started_at,
                            ended_at: runner.now(),
                        }
                    })
                })
                .collect();
            handles
                .into_iter()
                .filter_map(|handle| handle.join().ok())
                .collect()
        });

        for Ran {
            label,
            result,
            input,
            started_at,
            ended_at,
        } in results
        {
            let outcome = match result {
                Ok(artifact) => {
                    produced.insert(label.clone(), artifact.clone());
                    done.insert(label.clone());
                    Outcome::Done { artifact }
                }
                Err(detail) if cancel.load(Ordering::Relaxed) => {
                    let _ = detail;
                    Outcome::Cancelled
                }
                Err(detail) => Outcome::Failed { detail },
            };
            reports.insert(
                label.clone(),
                TaskReport {
                    label,
                    outcome,
                    input: Some(input),
                    started_at: Some(started_at),
                    ended_at: Some(ended_at),
                },
            );
        }
    }

    // Whatever is left never became ready. Why it did not is the question a
    // reader actually has, and the two causes lead different places.
    let stopped = cancel.load(Ordering::Relaxed);
    for task in plan.tasks() {
        if reports.contains_key(&task.label) {
            continue;
        }
        let outcome = if stopped {
            // The run was stopped. Saying "blocked by its dependency" would be
            // true and would send a reader to a task that was also cancelled,
            // looking for a fault that is not there.
            Outcome::Cancelled
        } else {
            Outcome::NotRun {
                blocked_by: task
                    .depends_on
                    .iter()
                    .filter(|need| !done.contains(*need))
                    .cloned()
                    .collect(),
            }
        };
        reports.insert(
            task.label.clone(),
            TaskReport {
                label: task.label.clone(),
                outcome,
                input: None,
                started_at: None,
                ended_at: None,
            },
        );
    }

    // Reported in the plan's own order, so two runs of the same plan read the
    // same way.
    let tasks: Vec<TaskReport> = plan
        .order()
        .iter()
        .filter_map(|label| reports.remove(label))
        .collect();
    let not_run = tasks
        .iter()
        .filter(|report| matches!(report.outcome, Outcome::NotRun { .. }))
        .map(|report| report.label.clone())
        .collect();

    GraphReport {
        complete: tasks.iter().all(|report| report.outcome.finished()),
        not_run,
        tasks,
    }
}

fn describe(error: &InputError) -> String {
    error.to_string()
}
