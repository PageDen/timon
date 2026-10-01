// Adapted for Timon. Not derived from Prodex source.
//! The task graph, and what makes one valid.
//!
//! **A dependency is an input, not an ordering.** The first draft of this plan
//! ordered tasks and stopped there, while the scheduler started every worker
//! from the hand-off's original commit. Those two together are broken: if task A
//! adds an API and task B builds a screen on it, running B after A leaves B
//! unable to see A's work. Ordering without inputs is a scheduler that
//! guarantees nothing, and Codex's review was right to say so.
//!
//! So a dependency carries content. What kind depends on what the consumer does:
//!
//! | Consumer | Receives |
//! |---|---|
//! | Reads | its dependencies' outputs as immutable artifacts, each named by producer, revision and digest |
//! | Writes | a host-prepared commit containing its accepted dependencies' changes |
//! | Independent | the run's base commit |
//!
//! Three rules keep that from becoming a hole of its own.
//!
//! **Integration is mechanical.** The host merges; it does not choose a side. A
//! conflict is a finding for the verifier, not something the host resolves by
//! picking.
//!
//! **Dependency content is data.** It cannot expand the consuming task's
//! permissions, budget, account access or authority. A task reading a
//! predecessor's output reads a string, not an instruction — which matters more
//! now that workers can write.
//!
//! **Artifacts are immutable.** A revised task produces a new revision rather
//! than mutating one already consumed, so what a descendant read stays knowable.
//! That is also what makes P6's invalidation possible: without it, "did this
//! change since B read it" has no answer.
//!
//! The host validates and refuses. It does not repair a plan, because repairing
//! one means deciding what the planner meant, and the host does not author work.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::{Deserialize, Serialize};

/// What a task does to the workspace, which decides what it receives.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Access {
    /// Reads only. Gets its dependencies' artifacts.
    #[default]
    Read,
    /// Changes code. Gets a commit containing its dependencies' changes.
    Write,
}

/// One task as the planner asked for it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Task {
    /// Names this task within the plan. Referenced by dependants.
    pub label: String,
    /// What the worker is asked to do, in the planner's words, unedited.
    pub task: String,
    /// Labels this task consumes. Order within the list is not significant.
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub access: Access,
    /// What must be true of the result for this task to have been done.
    ///
    /// Written by the planner before the work runs, which is the only time it
    /// can be written honestly: criteria invented afterwards describe what
    /// happened rather than what was wanted.
    #[serde(default)]
    pub acceptance: Vec<crate::acceptance::Criterion>,
}

/// What the planner produced.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Plan {
    pub tasks: Vec<Task>,
    /// The planner's own reasoning about the split, in its words.
    #[serde(default)]
    pub notes: String,
}

/// Limits a plan is held to. Set by the host, never by the planner.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub max_tasks: usize,
    /// Longest chain of dependencies. Depth is what turns a wide plan into a
    /// slow one, and a planner cannot feel the wall-clock cost of it.
    pub max_depth: usize,
    pub max_task_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_tasks: 12,
            max_depth: 4,
            max_task_bytes: 8 * 1024,
        }
    }
}

/// Why a plan was refused.
///
/// Every case names the task and says what would make it valid, because the
/// planner is the thing that has to fix it and it only sees what it is told.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "fault", rename_all = "snake_case")]
pub enum Invalid {
    NoTasks,
    TooManyTasks {
        asked: usize,
        limit: usize,
    },
    DuplicateLabel {
        label: String,
    },
    EmptyLabel {
        position: usize,
    },
    EmptyTask {
        label: String,
    },
    TaskTooLong {
        label: String,
        bytes: usize,
        limit: usize,
    },
    /// A dependency that is not a task in this plan.
    UnknownDependency {
        label: String,
        missing: String,
    },
    /// A task that depends on itself, directly or through others.
    Cycle {
        labels: Vec<String>,
    },
    TooDeep {
        depth: usize,
        limit: usize,
    },
    /// A task that depends on itself by name.
    SelfDependency {
        label: String,
    },
}

impl std::fmt::Display for Invalid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Invalid::NoTasks => write!(f, "the plan has no tasks"),
            Invalid::TooManyTasks { asked, limit } => write!(
                f,
                "the plan asks for {asked} tasks; this host allows {limit}. Combine \
                 the smallest ones or plan fewer"
            ),
            Invalid::DuplicateLabel { label } => write!(
                f,
                "two tasks are labelled {label:?}; a dependant could not say which it meant"
            ),
            Invalid::EmptyLabel { position } => {
                write!(f, "the task at position {position} has no label")
            }
            Invalid::EmptyTask { label } => {
                write!(f, "task {label:?} says nothing for a worker to do")
            }
            Invalid::TaskTooLong {
                label,
                bytes,
                limit,
            } => write!(
                f,
                "task {label:?} is {bytes} bytes; this host allows {limit}. A task \
                 that long is usually two tasks"
            ),
            Invalid::UnknownDependency { label, missing } => write!(
                f,
                "task {label:?} depends on {missing:?}, which is not a task in this \
                 plan. Add it, or drop the dependency"
            ),
            Invalid::Cycle { labels } => write!(
                f,
                "these tasks depend on each other in a circle: {}. One of them has to \
                 go first",
                labels.join(" → ")
            ),
            Invalid::TooDeep { depth, limit } => write!(
                f,
                "the longest chain of dependencies is {depth} deep; this host allows \
                 {limit}. Depth is what makes a plan slow, because each link waits \
                 for the one before it"
            ),
            Invalid::SelfDependency { label } => {
                write!(f, "task {label:?} depends on itself")
            }
        }
    }
}

/// A plan the host has checked and will schedule.
///
/// Only constructed by [`validate`], so holding one is proof it was checked.
#[derive(Clone, Debug, Serialize)]
pub struct Validated {
    tasks: Vec<Task>,
    /// Tasks in an order where every dependency comes before its dependants.
    order: Vec<String>,
    /// How deep the longest chain is, for reporting.
    depth: usize,
    pub notes: String,
}

impl Validated {
    pub fn tasks(&self) -> &[Task] {
        &self.tasks
    }

    /// Labels in dependency order.
    pub fn order(&self) -> &[String] {
        &self.order
    }

    pub fn depth(&self) -> usize {
        self.depth
    }

    pub fn task(&self, label: &str) -> Option<&Task> {
        self.tasks.iter().find(|task| task.label == label)
    }

    /// Tasks that can start at once: those whose dependencies are all done.
    ///
    /// Given rather than derived by the scheduler, so "what may run now" has one
    /// definition and not one per caller.
    pub fn ready(&self, done: &BTreeSet<String>) -> Vec<&Task> {
        self.tasks
            .iter()
            .filter(|task| {
                !done.contains(&task.label)
                    && task.depends_on.iter().all(|need| done.contains(need))
            })
            .collect()
    }

    /// Everything that consumes this task's output, directly or through others.
    ///
    /// What P6 invalidates when a task is revised. A descendant that read an
    /// artifact which has since changed is stale even though it succeeded.
    pub fn descendants(&self, label: &str) -> BTreeSet<String> {
        let mut found = BTreeSet::new();
        let mut frontier = vec![label.to_string()];
        while let Some(current) = frontier.pop() {
            for task in &self.tasks {
                if task.depends_on.contains(&current) && found.insert(task.label.clone()) {
                    frontier.push(task.label.clone());
                }
            }
        }
        found
    }
}

/// Checks a plan, or says everything wrong with it.
///
/// Returns *all* the faults rather than the first. A planner given one fault at
/// a time needs a round trip per fault, and each round trip is a model call.
pub fn validate(plan: &Plan, limits: &Limits) -> Result<Validated, Vec<Invalid>> {
    let mut faults = Vec::new();

    if plan.tasks.is_empty() {
        return Err(vec![Invalid::NoTasks]);
    }
    if plan.tasks.len() > limits.max_tasks {
        faults.push(Invalid::TooManyTasks {
            asked: plan.tasks.len(),
            limit: limits.max_tasks,
        });
    }

    let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
    for (position, task) in plan.tasks.iter().enumerate() {
        if task.label.trim().is_empty() {
            faults.push(Invalid::EmptyLabel { position });
            continue;
        }
        *seen.entry(task.label.as_str()).or_insert(0) += 1;
        if task.task.trim().is_empty() {
            faults.push(Invalid::EmptyTask {
                label: task.label.clone(),
            });
        }
        if task.task.len() > limits.max_task_bytes {
            faults.push(Invalid::TaskTooLong {
                label: task.label.clone(),
                bytes: task.task.len(),
                limit: limits.max_task_bytes,
            });
        }
    }
    for (label, count) in &seen {
        if *count > 1 {
            faults.push(Invalid::DuplicateLabel {
                label: (*label).to_string(),
            });
        }
    }

    let labels: BTreeSet<&str> = plan.tasks.iter().map(|task| task.label.as_str()).collect();
    for task in &plan.tasks {
        for need in &task.depends_on {
            if need == &task.label {
                faults.push(Invalid::SelfDependency {
                    label: task.label.clone(),
                });
            } else if !labels.contains(need.as_str()) {
                faults.push(Invalid::UnknownDependency {
                    label: task.label.clone(),
                    missing: need.clone(),
                });
            }
        }
    }

    // Ordering is attempted even when other faults exist, so a cycle is reported
    // in the same round as everything else.
    match topological(&plan.tasks) {
        Ok(order) => {
            let depth = depth_of(&plan.tasks);
            if depth > limits.max_depth {
                faults.push(Invalid::TooDeep {
                    depth,
                    limit: limits.max_depth,
                });
            }
            if faults.is_empty() {
                return Ok(Validated {
                    tasks: plan.tasks.clone(),
                    order,
                    depth,
                    notes: plan.notes.clone(),
                });
            }
        }
        Err(cycle) => faults.push(Invalid::Cycle { labels: cycle }),
    }

    Err(faults)
}

/// Dependency order, or the labels caught in a circle.
fn topological(tasks: &[Task]) -> Result<Vec<String>, Vec<String>> {
    let mut remaining: HashMap<&str, BTreeSet<&str>> = tasks
        .iter()
        .map(|task| {
            (
                task.label.as_str(),
                task.depends_on
                    .iter()
                    .map(String::as_str)
                    // A dependency outside the plan is reported separately; here
                    // it must not make everything look circular.
                    .filter(|need| tasks.iter().any(|t| t.label == *need))
                    .collect(),
            )
        })
        .collect();

    let mut order = Vec::new();
    while !remaining.is_empty() {
        // Sorted, so the same plan always orders the same way. A scheduler whose
        // order depends on hash iteration is a scheduler nobody can reproduce.
        let mut ready: Vec<&str> = remaining
            .iter()
            .filter(|(_, needs)| needs.is_empty())
            .map(|(label, _)| *label)
            .collect();
        ready.sort_unstable();

        if ready.is_empty() {
            let mut stuck: Vec<String> = remaining.keys().map(|l| l.to_string()).collect();
            stuck.sort();
            return Err(stuck);
        }
        for label in ready {
            remaining.remove(label);
            for needs in remaining.values_mut() {
                needs.remove(label);
            }
            order.push(label.to_string());
        }
    }
    Ok(order)
}

/// Longest chain of dependencies in the plan.
fn depth_of(tasks: &[Task]) -> usize {
    fn walk<'a>(
        label: &'a str,
        tasks: &'a [Task],
        seen: &mut BTreeSet<&'a str>,
        memo: &mut HashMap<&'a str, usize>,
    ) -> usize {
        if let Some(known) = memo.get(label) {
            return *known;
        }
        if !seen.insert(label) {
            return 0; // a cycle, reported elsewhere
        }
        let task = tasks.iter().find(|t| t.label == label);
        let deepest = task
            .map(|task| {
                task.depends_on
                    .iter()
                    .filter(|need| tasks.iter().any(|t| &t.label == *need))
                    .map(|need| walk(need, tasks, seen, memo))
                    .max()
                    .unwrap_or(0)
            })
            .unwrap_or(0);
        seen.remove(label);
        let depth = deepest + 1;
        memo.insert(label, depth);
        depth
    }

    let mut memo = HashMap::new();
    tasks
        .iter()
        .map(|task| {
            let mut seen = BTreeSet::new();
            walk(&task.label, tasks, &mut seen, &mut memo)
        })
        .max()
        .unwrap_or(0)
}
