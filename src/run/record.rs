// Adapted for Timon. Not derived from Prodex source.
//! The run record and its store.
//!
//! SQLite, with the same durability settings as the usage recorder: a run that
//! survives a crash is the entire point, and a record written to a page cache
//! that never reached the disk would survive nothing.

use std::path::{Path, PathBuf};

use rusqlite::Connection;
use serde::{Deserialize, Serialize};

/// Where a run has got to.
///
/// `Interrupted` is not a failure. It means the orchestrator stopped while this
/// run was going and nobody can now say what happened to the work. Recording
/// that honestly is better than leaving a run `Running` forever, which would
/// make every status display a lie after the first crash.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Admitted and executing.
    Running,
    /// Finished, whatever the verdict on the work.
    Finished,
    /// Cancellation requested; workers are being stopped.
    Cancelling,
    /// Cancellation completed.
    Cancelled,
    /// The orchestrator stopped while this run was going.
    Interrupted,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Running => "running",
            Status::Finished => "finished",
            Status::Cancelling => "cancelling",
            Status::Cancelled => "cancelled",
            Status::Interrupted => "interrupted",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "running" => Status::Running,
            "finished" => Status::Finished,
            "cancelling" => Status::Cancelling,
            "cancelled" => Status::Cancelled,
            "interrupted" => Status::Interrupted,
            _ => return None,
        })
    }

    /// True when this run is still going, and so is a candidate for recovery or
    /// cancellation.
    pub fn live(self) -> bool {
        matches!(self, Status::Running | Status::Cancelling)
    }
}

/// What the run started from.
///
/// Named rather than implied. "The base commit" is ambiguous in a repository
/// with uncommitted work, and a report that cannot say what was included is a
/// report nobody can reproduce from.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Base {
    /// The commit at HEAD, with the working tree clean.
    Head { commit: String },
    /// The commit at HEAD plus a snapshot of uncommitted work.
    Snapshot { commit: String, snapshot: String },
    /// Not a repository. Research and other non-code work.
    None,
}

impl Base {
    pub fn commit(&self) -> Option<&str> {
        match self {
            Base::Head { commit } | Base::Snapshot { commit, .. } => Some(commit),
            Base::None => None,
        }
    }

    /// How an operator should read this, in one line.
    pub fn describe(&self) -> String {
        match self {
            Base::Head { commit } => format!("{} (clean working tree)", short(commit)),
            Base::Snapshot { commit, snapshot } => format!(
                "{} plus uncommitted work, snapshot {}",
                short(commit),
                short(snapshot)
            ),
            Base::None => "not a repository".to_string(),
        }
    }
}

fn short(commit: &str) -> String {
    commit.chars().take(12).collect()
}

/// One hand-off, as it is remembered.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Run {
    pub id: String,
    /// The uid that asked for the work, from the kernel. Attribution, and the
    /// only principal allowed to cancel it.
    pub principal_uid: u32,
    pub goal: String,
    /// The repository the work is rooted in, when there is one.
    pub workspace: Option<PathBuf>,
    pub base: Base,
    /// Pooled accounts this run may spend. Empty means the broker's own choice.
    pub accounts: Vec<String>,
    /// Attempts and token ceiling, which the admission ledger enforces.
    pub max_attempts: u32,
    pub token_ceiling: Option<u64>,
    /// Wall-clock deadline, as unix seconds.
    pub deadline: Option<i64>,
    pub status: Status,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    /// Set when the run stopped for a reason worth stating.
    pub detail: Option<String>,
    /// Supplied by the caller to make submission idempotent.
    pub submission_key: Option<String>,
}

impl Run {
    /// Whether this run's deadline has passed.
    pub fn overdue(&self, now: i64) -> bool {
        self.deadline.is_some_and(|deadline| now >= deadline)
    }
}

/// Why a run could not be recorded or read.
#[derive(Debug)]
pub enum RunError {
    Io(PathBuf, std::io::Error),
    Db(rusqlite::Error),
    /// A run was asked for by an id that is not there.
    Unknown(String),
    /// Cancellation asked for by somebody other than the run's principal.
    NotYours {
        run: String,
        principal_uid: u32,
    },
    Malformed(&'static str),
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunError::Io(path, error) => write!(f, "{}: {error}", path.display()),
            RunError::Db(error) => write!(f, "{error}"),
            RunError::Unknown(id) => write!(f, "no run {id}"),
            RunError::NotYours { run, principal_uid } => write!(
                f,
                "run {run} belongs to uid {principal_uid}; only its own principal can cancel it"
            ),
            RunError::Malformed(what) => write!(f, "{what}"),
        }
    }
}

impl std::error::Error for RunError {}

impl From<rusqlite::Error> for RunError {
    fn from(error: rusqlite::Error) -> Self {
        RunError::Db(error)
    }
}

/// The run store.
pub struct Runs {
    conn: Connection,
    path: PathBuf,
}

impl Runs {
    /// Opens the store, creating it if needed.
    ///
    /// `synchronous = FULL` because a run record exists to survive a crash, and
    /// the crash is exactly when a buffered write is lost.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, RunError> {
        let path = path.into();
        if let Some(parent) = path.parent()
            && !parent.exists()
        {
            let mut builder = std::fs::DirBuilder::new();
            builder.recursive(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            builder
                .create(parent)
                .map_err(|error| RunError::Io(parent.to_path_buf(), error))?;
        }
        let conn = Connection::open(&path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Runs { conn, path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Records a new run.
    ///
    /// A submission key that has been seen before returns the run it started
    /// rather than starting another. A hand-off resubmitted because the first
    /// reply was lost is one piece of work, not two, and doing it twice would
    /// spend twice.
    pub fn start(&self, run: &Run) -> Result<Run, RunError> {
        if let Some(key) = &run.submission_key
            && let Some(existing) = self.by_submission_key(key)?
        {
            return Ok(existing);
        }
        self.conn.execute(
            "INSERT INTO runs (
                 id, principal_uid, goal, workspace, base, accounts, max_attempts,
                 token_ceiling, deadline, status, started_at, ended_at, detail, submission_key
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            rusqlite::params![
                run.id,
                run.principal_uid,
                run.goal,
                run.workspace.as_ref().map(|p| p.display().to_string()),
                serde_json::to_string(&run.base).map_err(|_| RunError::Malformed("base"))?,
                serde_json::to_string(&run.accounts)
                    .map_err(|_| RunError::Malformed("accounts"))?,
                run.max_attempts,
                // SQLite has no unsigned integer. A ceiling is a token count,
                // so it cannot come close to the signed range; the clamp is
                // there to be explicit rather than because it can happen.
                run.token_ceiling
                    .map(|ceiling| ceiling.min(i64::MAX as u64) as i64),
                run.deadline,
                run.status.as_str(),
                run.started_at,
                run.ended_at,
                run.detail,
                run.submission_key,
            ],
        )?;
        Ok(run.clone())
    }

    /// Moves a run to a new status, recording when and why.
    pub fn settle(
        &self,
        id: &str,
        status: Status,
        now: i64,
        detail: Option<&str>,
    ) -> Result<(), RunError> {
        let ended = if status.live() { None } else { Some(now) };
        let changed = self.conn.execute(
            "UPDATE runs SET status = ?2, ended_at = ?3, detail = COALESCE(?4, detail)
             WHERE id = ?1",
            rusqlite::params![id, status.as_str(), ended, detail],
        )?;
        if changed == 0 {
            return Err(RunError::Unknown(id.to_string()));
        }
        Ok(())
    }

    /// Asks for a run to stop.
    ///
    /// Only its own principal may. A run belongs to the person who started it,
    /// and on a shared host "who asked" is the whole basis of that.
    pub fn cancel(&self, id: &str, by_uid: u32, now: i64) -> Result<Run, RunError> {
        let run = self.get(id)?;
        if run.principal_uid != by_uid {
            return Err(RunError::NotYours {
                run: id.to_string(),
                principal_uid: run.principal_uid,
            });
        }
        if !run.status.live() {
            return Ok(run);
        }
        self.settle(
            id,
            Status::Cancelling,
            now,
            Some("cancelled by its principal"),
        )?;
        self.get(id)
    }

    pub fn get(&self, id: &str) -> Result<Run, RunError> {
        self.conn
            .query_row("SELECT * FROM runs WHERE id = ?1", [id], |row| {
                Ok(read_row(row))
            })
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => RunError::Unknown(id.to_string()),
                other => RunError::Db(other),
            })?
    }

    fn by_submission_key(&self, key: &str) -> Result<Option<Run>, RunError> {
        let found = self
            .conn
            .query_row(
                "SELECT * FROM runs WHERE submission_key = ?1 ORDER BY started_at DESC LIMIT 1",
                [key],
                |row| Ok(read_row(row)),
            )
            .map(Some)
            .or_else(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(RunError::Db(other)),
            })?;
        found.transpose()
    }

    /// Runs in a given state, newest first.
    pub fn with_status(&self, status: Status) -> Result<Vec<Run>, RunError> {
        let mut statement = self
            .conn
            .prepare("SELECT * FROM runs WHERE status = ?1 ORDER BY started_at DESC")?;
        let rows = statement.query_map([status.as_str()], |row| Ok(read_row(row)))?;
        let mut runs = Vec::new();
        for row in rows {
            runs.push(row??);
        }
        Ok(runs)
    }

    /// The most recent runs, whatever their state.
    pub fn recent(&self, limit: usize) -> Result<Vec<Run>, RunError> {
        let mut statement = self
            .conn
            .prepare("SELECT * FROM runs ORDER BY started_at DESC LIMIT ?1")?;
        let rows = statement.query_map([limit as i64], |row| Ok(read_row(row)))?;
        let mut runs = Vec::new();
        for row in rows {
            runs.push(row??);
        }
        Ok(runs)
    }

    /// Marks every still-live run as interrupted.
    ///
    /// Called once at start-up. Any run the store still calls `running` cannot
    /// be running, because the process that was running it is gone. Saying so is
    /// the difference between a status display that is wrong forever and one
    /// that tells a developer their work stopped.
    ///
    /// Returns what was marked, so the caller can report it rather than swallow
    /// it.
    pub fn interrupt_stale(&self, now: i64) -> Result<Vec<Run>, RunError> {
        let live: Vec<Run> = self
            .with_status(Status::Running)?
            .into_iter()
            .chain(self.with_status(Status::Cancelling)?)
            .collect();
        for run in &live {
            self.settle(
                &run.id,
                Status::Interrupted,
                now,
                Some("the orchestrator stopped while this run was going"),
            )?;
        }
        Ok(live)
    }
}

/// Reads one row, tolerating a record written by a later version.
fn read_row(row: &rusqlite::Row<'_>) -> Result<Run, RunError> {
    let base: String = row.get("base")?;
    let accounts: String = row.get("accounts")?;
    let status: String = row.get("status")?;
    Ok(Run {
        id: row.get("id")?,
        principal_uid: row.get("principal_uid")?,
        goal: row.get("goal")?,
        workspace: row
            .get::<_, Option<String>>("workspace")?
            .map(PathBuf::from),
        base: serde_json::from_str(&base).map_err(|_| RunError::Malformed("base is not JSON"))?,
        accounts: serde_json::from_str(&accounts)
            .map_err(|_| RunError::Malformed("accounts is not JSON"))?,
        max_attempts: row.get("max_attempts")?,
        token_ceiling: row
            .get::<_, Option<i64>>("token_ceiling")?
            .map(|ceiling| ceiling.max(0) as u64),
        deadline: row.get("deadline")?,
        status: Status::parse(&status).ok_or(RunError::Malformed("unknown status"))?,
        started_at: row.get("started_at")?,
        ended_at: row.get("ended_at")?,
        detail: row.get("detail")?,
        submission_key: row.get("submission_key")?,
    })
}

impl Runs {
    /// Records what triage decided for a run.
    ///
    /// Kept in its own table rather than as columns on the run: P3 will make one
    /// decision per task, and a shape that only fits one decision would have to
    /// be undone then.
    ///
    /// The reasons are the point. Whether a route was the right one is a
    /// question about the alternative that was not run, and nothing downstream
    /// can answer it — so the evidence has to be here, or misrouting is not
    /// measurable at all.
    pub fn record_triage(
        &self,
        run_id: &str,
        decision: &crate::triage::Decision,
        now: i64,
    ) -> Result<(), RunError> {
        self.conn.execute(
            "INSERT INTO triage (run_id, task_label, route, reasons, signals, decided_at)
             VALUES (?1, NULL, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                run_id,
                decision.route.as_str(),
                serde_json::to_string(&decision.reasons)
                    .map_err(|_| RunError::Malformed("reasons"))?,
                serde_json::to_string(&decision.signals)
                    .map_err(|_| RunError::Malformed("signals"))?,
                now,
            ],
        )?;
        Ok(())
    }

    /// What triage decided for a run, newest first.
    pub fn triage_of(&self, run_id: &str) -> Result<Vec<crate::triage::Decision>, RunError> {
        let mut statement = self.conn.prepare(
            "SELECT route, reasons, signals FROM triage WHERE run_id = ?1
             ORDER BY decided_at DESC",
        )?;
        let rows = statement.query_map([run_id], |row| {
            let route: String = row.get(0)?;
            let reasons: String = row.get(1)?;
            let signals: String = row.get(2)?;
            Ok((route, reasons, signals))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (route, reasons, signals) = row?;
            out.push(crate::triage::Decision {
                route: crate::triage::Route::parse(&route)
                    .ok_or(RunError::Malformed("unknown route"))?,
                reasons: serde_json::from_str(&reasons)
                    .map_err(|_| RunError::Malformed("reasons are not JSON"))?,
                signals: serde_json::from_str(&signals)
                    .map_err(|_| RunError::Malformed("signals are not JSON"))?,
            });
        }
        Ok(out)
    }
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS runs (
    id             TEXT PRIMARY KEY,
    principal_uid  INTEGER NOT NULL,
    goal           TEXT NOT NULL,
    workspace      TEXT,
    base           TEXT NOT NULL,
    accounts       TEXT NOT NULL,
    max_attempts   INTEGER NOT NULL,
    token_ceiling  INTEGER,
    deadline       INTEGER,
    status         TEXT NOT NULL,
    started_at     INTEGER NOT NULL,
    ended_at       INTEGER,
    detail         TEXT,
    submission_key TEXT
);
CREATE INDEX IF NOT EXISTS runs_status ON runs (status);
CREATE INDEX IF NOT EXISTS runs_started ON runs (started_at DESC);
-- One submission key admits one run. This is what makes a resubmitted hand-off
-- the same piece of work rather than a second one that spends twice.
CREATE UNIQUE INDEX IF NOT EXISTS runs_submission_key
    ON runs (submission_key) WHERE submission_key IS NOT NULL;

-- One row per routing decision. `task_label` is null for the run's own
-- decision and will name a task once P3 routes each one.
CREATE TABLE IF NOT EXISTS triage (
    run_id      TEXT NOT NULL,
    task_label  TEXT,
    route       TEXT NOT NULL,
    reasons     TEXT NOT NULL,
    signals     TEXT NOT NULL,
    decided_at  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS triage_run ON triage (run_id);
";
