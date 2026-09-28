// Adapted for Timon. Not derived from Prodex source.
//! What a task receives from the tasks it depends on.
//!
//! This is the half of P3 that makes a dependency mean something. Ordering says
//! B runs after A; this says what B can see of A.
//!
//! Artifacts are **immutable and named by digest**. A revised task produces a new
//! revision rather than changing one already consumed, which is what makes two
//! later questions answerable: what exactly did B read, and has it changed since.
//! Without that, P6's invalidation has nothing to compare.

use serde::{Deserialize, Serialize};

/// Something a task produced, as its dependants will see it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artifact {
    /// The task that produced it.
    pub producer: String,
    /// Which attempt at that task. A repair produces revision 2, not a changed 1.
    pub revision: u32,
    /// SHA-256 of the content, so "has this changed" is a comparison and not a
    /// judgement.
    pub digest: String,
    pub content: String,
}

impl Artifact {
    pub fn new(producer: &str, revision: u32, content: impl Into<String>) -> Self {
        let content = content.into();
        Artifact {
            producer: producer.to_string(),
            revision,
            digest: digest_of(&content),
            content,
        }
    }

    /// How a dependant refers to it: producer, revision and digest, never the
    /// content. Two artifacts with the same name are the same bytes.
    pub fn name(&self) -> String {
        format!("{}@{}#{}", self.producer, self.revision, &self.digest[..12])
    }
}

/// SHA-256, hex.
pub fn digest_of(content: &str) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, content.as_bytes());
    use std::fmt::Write;
    digest.as_ref().iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

/// What a task starts from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Input {
    /// Nothing to consume: the run's base commit, as an independent task gets.
    Base { commit: Option<String> },
    /// A reading task's dependencies, as immutable artifacts.
    Artifacts { from: Vec<Artifact> },
    /// A writing task's starting commit, prepared by the host to contain its
    /// dependencies' accepted changes.
    ///
    /// `merged` names what went into it, so a report can say what the task could
    /// see without anyone re-deriving it from the commit.
    PreparedCommit { commit: String, merged: Vec<String> },
}

/// Why the inputs for a task could not be assembled.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InputError {
    /// A dependency has not produced anything yet. The scheduler ran something
    /// out of order, which is a host fault rather than a planner one.
    NotReady { label: String, missing: String },
    /// Two writing dependencies changed the same thing and the host will not
    /// choose between them.
    Conflict {
        label: String,
        between: Vec<String>,
        detail: String,
    },
}

impl std::fmt::Display for InputError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InputError::NotReady { label, missing } => write!(
                f,
                "task {label:?} was started before {missing:?} produced anything"
            ),
            InputError::Conflict {
                label,
                between,
                detail,
            } => write!(
                f,
                "preparing the start state for {label:?} found a conflict between {}: \
                 {detail}. The host merges; it does not choose a side, so this is a \
                 finding for the verifier",
                between.join(" and ")
            ),
        }
    }
}

impl std::error::Error for InputError {}

/// Assembles what one task receives.
///
/// `produced` is what has been produced so far, by label. `prepare` is how the
/// host makes a commit containing a set of changes — the git work, which is the
/// scheduler's to provide because only it knows where the worktrees are.
pub fn inputs_for(
    task: &crate::dag::Task,
    base: Option<&str>,
    produced: &std::collections::BTreeMap<String, Artifact>,
    prepare: impl Fn(&[String]) -> Result<String, String>,
) -> Result<Input, InputError> {
    if task.depends_on.is_empty() {
        return Ok(Input::Base {
            commit: base.map(str::to_string),
        });
    }

    let mut from = Vec::new();
    for need in &task.depends_on {
        let artifact = produced.get(need).ok_or_else(|| InputError::NotReady {
            label: task.label.clone(),
            missing: need.clone(),
        })?;
        from.push(artifact.clone());
    }

    match task.access {
        // A reading task gets the content, and nothing it reads can widen what
        // it may do: an artifact is a string, not an instruction.
        crate::dag::Access::Read => Ok(Input::Artifacts { from }),
        // A writing task needs its predecessors' changes actually present, not
        // described. Ordering alone would leave it editing code that does not
        // have the API it was told to build on.
        crate::dag::Access::Write => {
            let merged: Vec<String> = task.depends_on.clone();
            match prepare(&merged) {
                Ok(commit) => Ok(Input::PreparedCommit { commit, merged }),
                Err(detail) => Err(InputError::Conflict {
                    label: task.label.clone(),
                    between: merged,
                    detail,
                }),
            }
        }
    }
}

/// What a task saw, recorded so a later revision can tell whether it still holds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Consumed {
    pub label: String,
    /// Artifact names, in the form `producer@revision#digest`.
    pub artifacts: Vec<String>,
}

/// True when what a task consumed no longer matches what its dependencies now
/// produce.
///
/// The question P6 asks before reusing a successful task. Compared by digest, so
/// a dependency that was re-run and produced identical output does not
/// invalidate anything — re-running is not the same as changing.
pub fn stale(consumed: &Consumed, produced: &std::collections::BTreeMap<String, Artifact>) -> bool {
    consumed.artifacts.iter().any(|name| {
        let Some((producer, _)) = name.split_once('@') else {
            return true;
        };
        match produced.get(producer) {
            Some(current) => current.name() != *name,
            None => true,
        }
    })
}
