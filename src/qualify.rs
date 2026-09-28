// Adapted for Timon. Not derived from Prodex source.
//! Qualifying the write sandbox before any worker writes.
//!
//! P4.3. The plan makes this a gate rather than a checklist: writing workers
//! stay disabled until it passes, because a worker that changes code is the
//! first thing in this project that can damage something.
//!
//! **A worktree is a workflow boundary, not a security boundary.** Worktrees of
//! one repository share `.git` — refs, hooks, config, object store — and a
//! repository hook is code that runs with the worker's permissions. So the
//! checks are about what the sandbox actually stops, not about which directory
//! the worker was told to stay in.
//!
//! Each check is a fact about the host, measured by attempting the thing. A
//! check that passes because nobody tried hard enough is worse than no check,
//! so every probe here is a real attempt and its result is compared against the
//! filesystem afterwards rather than against what the attempt reported.

use serde::Serialize;

/// One thing the sandbox must or must not allow.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Check {
    /// A worker can do its job at all.
    WriteInsideWorktree,
    /// The developer's tree, other worktrees, home directories, paths not given.
    WriteOutsideWorktree,
    /// Refs, config, hooks, another worktree's state.
    ModifySharedGitMetadata,
    /// Branch creation and integration belong to the host.
    MoveRefs,
    /// Hooks and build scripts run with the worker's permissions, not the host's.
    RunRepositoryHooks,
    /// Push, fetch, or otherwise reach a remote.
    ReachGitRemote,
    /// The broker's store, `~/.codex`, anything holding a token.
    ReadCredentials,
    /// Files belonging to another account on the host.
    ReadAnotherAccount,
    /// Processes that outlive the worker.
    LeaveProcessesBehind,
}

impl Check {
    pub fn all() -> [Check; 9] {
        [
            Check::WriteInsideWorktree,
            Check::WriteOutsideWorktree,
            Check::ModifySharedGitMetadata,
            Check::MoveRefs,
            Check::RunRepositoryHooks,
            Check::ReachGitRemote,
            Check::ReadCredentials,
            Check::ReadAnotherAccount,
            Check::LeaveProcessesBehind,
        ]
    }

    /// What the sandbox is supposed to do about it.
    pub fn expected(self) -> Expected {
        match self {
            Check::WriteInsideWorktree => Expected::Allowed,
            _ => Expected::Blocked,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Check::WriteInsideWorktree => "write inside the worktree",
            Check::WriteOutsideWorktree => "write outside the worktree",
            Check::ModifySharedGitMetadata => "modify shared git metadata",
            Check::MoveRefs => "create branches or move refs",
            Check::RunRepositoryHooks => "run repository hooks",
            Check::ReachGitRemote => "reach a git remote",
            Check::ReadCredentials => "read a credential store",
            Check::ReadAnotherAccount => "read another account's files",
            Check::LeaveProcessesBehind => "leave processes behind",
        }
    }

    /// Why this one is in the list, in the terms that made it a check.
    pub fn why(self) -> &'static str {
        match self {
            Check::WriteInsideWorktree => {
                "a worker that cannot write cannot do the work it was given"
            }
            Check::WriteOutsideWorktree => {
                "the developer's working tree is not the worker's to change, and a \
                 run must stay discardable"
            }
            Check::ModifySharedGitMetadata => {
                "worktrees share one .git; a worker confined to its directory can \
                 still reach every other worktree through it"
            }
            Check::MoveRefs => {
                "the host owns branch creation and integration, so a worker that \
                 moves refs has taken a decision that is not its own"
            }
            Check::RunRepositoryHooks => {
                "a hook is code in the repository that runs on ordinary git \
                 commands, so it is a way to execute anything the worker wants"
            }
            Check::ReachGitRemote => {
                "pushing puts a worker's changes somewhere the developer never \
                 reviewed"
            }
            Check::ReadCredentials => {
                "the whole credential design rests on the broker being the only \
                 holder; a worker that can read the store makes that untrue"
            }
            Check::ReadAnotherAccount => {
                "a shared host means somebody else's work is on the same disk"
            }
            Check::LeaveProcessesBehind => {
                "a process that outlives its run keeps spending and holds its \
                 output pipes open"
            }
        }
    }
}

/// What the sandbox should do.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Expected {
    Allowed,
    Blocked,
}

/// What it actually did.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Observed {
    Allowed,
    Blocked,
    /// The probe could not be run, so nothing was learned.
    ///
    /// Never counted as a pass. An unrun check is an unknown, and unknown is
    /// not zero.
    NotRun,
}

/// One check and what came of it.
#[derive(Clone, Debug, Serialize)]
pub struct Finding {
    pub check: Check,
    pub expected: Expected,
    pub observed: Observed,
    /// What was attempted, so a reader can repeat it by hand.
    pub probe: String,
    /// What was seen afterwards on the filesystem, which is the evidence. A
    /// probe that reports its own success is not evidence.
    pub evidence: String,
}

impl Finding {
    pub fn passed(&self) -> bool {
        matches!(
            (self.expected, self.observed),
            (Expected::Allowed, Observed::Allowed) | (Expected::Blocked, Observed::Blocked)
        )
    }
}

/// The gate's verdict.
#[derive(Clone, Debug, Serialize)]
pub struct Qualification {
    pub findings: Vec<Finding>,
    pub host: String,
    pub taken_at: i64,
}

impl Qualification {
    /// True only when every check was run and every one passed.
    ///
    /// A check that could not be run keeps the gate shut. The alternative is a
    /// gate that opens because a probe was broken, which is the failure mode
    /// worth designing against.
    pub fn passed(&self) -> bool {
        self.findings.len() == Check::all().len() && self.findings.iter().all(Finding::passed)
    }

    pub fn failures(&self) -> Vec<&Finding> {
        self.findings.iter().filter(|f| !f.passed()).collect()
    }

    /// For a person, in the order the checks are defined.
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "Write-sandbox qualification on {}\n  taken {}\n\n",
            self.host,
            crate::recorder::render::utc(self.taken_at)
        ));
        for finding in &self.findings {
            let mark = if finding.passed() { "ok  " } else { "FAIL" };
            let observed = match finding.observed {
                Observed::Allowed => "allowed",
                Observed::Blocked => "blocked",
                Observed::NotRun => "not run",
            };
            out.push_str(&format!(
                "  {mark} {:<32} {observed}\n",
                finding.check.as_str()
            ));
            if !finding.passed() {
                out.push_str(&format!(
                    "         why it matters: {}\n",
                    finding.check.why()
                ));
                out.push_str(&format!("         evidence: {}\n", finding.evidence));
            }
        }
        out.push('\n');
        if self.passed() {
            out.push_str("PASSED. Writing workers may be enabled.\n");
        } else {
            out.push_str(
                "NOT PASSED. Writing workers stay disabled: a worker that changes \n\
                 code is the first thing here that can damage something, and this \n\
                 is the check that says it cannot.\n",
            );
        }
        out
    }
}

/// Whether writing workers may run on this host.
///
/// Read wherever a writing task would be started. The gate is not advisory: the
/// plan says the sandbox is qualified *before* any worker writes, and a flag
/// somebody can forget to check is not a gate.
pub fn writing_permitted(qualification: Option<&Qualification>) -> Result<(), String> {
    match qualification {
        Some(q) if q.passed() => Ok(()),
        Some(q) => Err(format!(
            "the write sandbox has not qualified on this host: {}. \
             Writing workers are disabled until it does",
            q.failures()
                .iter()
                .map(|f| f.check.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
        None => Err(
            "the write sandbox has never been qualified on this host. Run \
             `timon qualify write-sandbox`; writing workers are disabled until it passes"
                .to_string(),
        ),
    }
}
