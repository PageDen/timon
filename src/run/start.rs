// Adapted for Timon. Not derived from Prodex source.
//! Starting a run: what is decided before any model is called.
//!
//! Preflight is the whole point. A request that cannot succeed — no free slot,
//! an exhausted ceiling, a deadline already passed — is refused here, before a
//! token is spent, with the reason said plainly. Discovering the same fact after
//! paying for it is the failure this prevents.

use std::path::{Path, PathBuf};

use super::record::{Base, Run, RunError, Runs, Status};

/// Why a run was not started.
///
/// Each case names what the caller can do about it. A refusal that only says
/// "no" makes somebody read the source to find out why.
#[derive(Debug)]
pub enum Refused {
    /// Every host-wide worker slot is taken.
    NoSlot {
        limit: u16,
    },
    /// The deadline had already passed when the run was submitted.
    DeadlinePassed {
        deadline: i64,
        now: i64,
    },
    /// A ceiling below one attempt's reservation can never admit anything.
    CeilingBelowOneAttempt {
        ceiling: u64,
        reserve: u64,
    },
    /// The workspace is not a directory, or not readable.
    Workspace {
        path: PathBuf,
        why: String,
    },
    Store(RunError),
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refused::NoSlot { limit } => write!(
                f,
                "all {limit} host worker slots are in use; this run was not started. \
                 `timon slots report` shows who holds them"
            ),
            Refused::DeadlinePassed { deadline, now } => write!(
                f,
                "the deadline passed {}s before this run was submitted",
                now - deadline
            ),
            Refused::CeilingBelowOneAttempt { ceiling, reserve } => write!(
                f,
                "a token ceiling of {ceiling} cannot admit even one attempt, which \
                 reserves {reserve}; the run would be refused at its first task"
            ),
            Refused::Workspace { path, why } => {
                write!(f, "{}: {why}", path.display())
            }
            Refused::Store(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for Refused {}

/// What the caller asked for.
pub struct Request {
    pub goal: String,
    pub principal_uid: u32,
    pub workspace: Option<PathBuf>,
    pub accounts: Vec<String>,
    pub max_attempts: u32,
    pub token_ceiling: Option<u64>,
    /// Seconds from now, converted to an absolute deadline on admission.
    pub deadline: Option<i64>,
    pub submission_key: Option<String>,
}

/// Checks everything that can be checked before spending, then records the run.
///
/// Order matters: the cheapest and most certain refusals come first, so a run
/// that was never going to start does not take a slot on its way to being
/// refused.
pub fn admit(
    runs: &Runs,
    request: Request,
    base: Base,
    now: i64,
    slots_free: Option<(u16, u16)>,
    attempt_reserve: u64,
) -> Result<Run, Refused> {
    if let Some(deadline) = request.deadline
        && deadline <= now
    {
        return Err(Refused::DeadlinePassed { deadline, now });
    }

    // A ceiling too small for one attempt is a request that cannot succeed, and
    // saying so now is kinder than admitting it and refusing its first task.
    if let Some(ceiling) = request.token_ceiling
        && ceiling < attempt_reserve
    {
        return Err(Refused::CeilingBelowOneAttempt {
            ceiling,
            reserve: attempt_reserve,
        });
    }

    if let Some(workspace) = &request.workspace {
        check_workspace(workspace)?;
    }

    if let Some((used, limit)) = slots_free
        && used >= limit
    {
        return Err(Refused::NoSlot { limit });
    }

    let run = Run {
        id: new_id(now),
        principal_uid: request.principal_uid,
        goal: request.goal,
        workspace: request.workspace,
        base,
        accounts: request.accounts,
        max_attempts: request.max_attempts,
        token_ceiling: request.token_ceiling,
        deadline: request.deadline,
        status: Status::Running,
        started_at: now,
        ended_at: None,
        detail: None,
        submission_key: request.submission_key,
    };
    runs.start(&run).map_err(Refused::Store)
}

fn check_workspace(path: &Path) -> Result<(), Refused> {
    let metadata = std::fs::metadata(path).map_err(|error| Refused::Workspace {
        path: path.to_path_buf(),
        why: format!("cannot be read: {error}"),
    })?;
    if !metadata.is_dir() {
        return Err(Refused::Workspace {
            path: path.to_path_buf(),
            why: "is not a directory".to_string(),
        });
    }
    Ok(())
}

/// A run id: sortable by time, and unique without coordination.
///
/// Time first so `recent` reads in order and an operator can see at a glance
/// when a run started; random suffix so two runs admitted in the same second
/// cannot collide.
pub fn new_id(now: i64) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.subsec_nanos())
        .unwrap_or_default();
    let pid = std::process::id();
    format!("run-{now}-{:04x}{:04x}", nanos & 0xffff, pid & 0xffff)
}
