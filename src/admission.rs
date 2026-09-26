// Adapted for Timon. Not derived from Prodex source.
//! Per-run admission: how much a run is allowed to start.
//!
//! Two kinds of limit live here and they are not equivalent, so they are never
//! reported as though they were.
//!
//! An attempt count is **enforced**. The ledger is the only place attempts are
//! admitted from, so refusing one refuses it.
//!
//! A token ceiling is an **admission estimate**. It decides whether to start
//! another attempt given what has been observed and what is still outstanding.
//! It cannot bound what an attempt spends once running: the numbers arrive after
//! the work, and a reservation is a guess at a maximum, not a maximum. Overshoot
//! is possible. No deterministic token-spend bound is claimed anywhere in here,
//! and the word "cap" is deliberately absent.
//!
//! Unknown usage keeps its full reservation rather than settling to zero. An
//! attempt whose numbers never arrived may have spent anything, and treating
//! that as nothing would let a run admit work it cannot account for.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Reservation used when nothing better is known.
///
/// A real deployment replaces this with the p95 of observed per-attempt usage
/// from its own qualification runs. It is never zero: a zero reservation would
/// admit unlimited concurrent attempts against any ceiling.
pub const DEFAULT_ATTEMPT_RESERVE: u64 = 20_000;

/// What a run is allowed.
#[derive(Clone, Copy, Debug)]
pub struct RunLimits {
    /// Attempts this run may start. Enforced.
    pub max_attempts: u32,
    /// Token admission ceiling, if one is set. An estimate, not a cap.
    pub token_ceiling: Option<u64>,
    /// Tokens held for an attempt until its real usage is known.
    pub attempt_reserve: u64,
}

impl Default for RunLimits {
    fn default() -> Self {
        RunLimits {
            max_attempts: 64,
            token_ceiling: None,
            attempt_reserve: DEFAULT_ATTEMPT_RESERVE,
        }
    }
}

/// Why an attempt was not admitted.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum Refusal {
    /// The run has started as many attempts as it is allowed.
    AttemptsExhausted { started: u32, max_attempts: u32 },
    /// Admitting another attempt's reservation would pass the ceiling.
    ///
    /// `unsettled` is the part of `committed` still held for attempts whose
    /// usage is not known. It is the reason a run can be refused while appearing
    /// to have room: those tokens may already have been spent.
    CeilingWouldBeExceeded {
        committed: u64,
        unsettled: u64,
        requested: u64,
        ceiling: u64,
    },
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::AttemptsExhausted {
                started,
                max_attempts,
            } => write!(
                f,
                "this run has started {started} of {max_attempts} attempts"
            ),
            Refusal::CeilingWouldBeExceeded {
                committed,
                unsettled,
                requested,
                ceiling,
            } => write!(
                f,
                "admitting {requested} more tokens would pass the run's admission ceiling of \
{ceiling}: {committed} committed, of which {unsettled} is held for attempts whose usage is \
not known"
            ),
        }
    }
}

/// An admitted attempt. Settling it is the caller's job.
#[derive(Clone, Debug, Serialize)]
pub struct Admitted {
    pub attempt_id: String,
    /// Tokens held for this attempt until its usage is known.
    pub reserved: u64,
    pub committed_after: u64,
    /// Stated on every admission so it cannot be read as a guarantee.
    pub basis: &'static str,
}

/// What a token ceiling is, said wherever one is reported.
pub const CEILING_BASIS: &str = "A token admission ceiling, not a cap: it decides whether to \
start another attempt, and cannot bound what a running attempt spends. Overshoot is possible \
and no deterministic token-spend bound is claimed.";

/// One run's shared ledger.
#[derive(Clone, Debug)]
pub struct Ledger {
    path: PathBuf,
    lock_path: PathBuf,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct State {
    run_id: String,
    attempts: Vec<Entry>,
}

/// How far along an attempt is, and what it is known to have cost.
///
/// Written as three named states rather than nested options. `Option<Option<u64>>`
/// would have been the obvious encoding and is a trap: JSON has one `null`, so
/// "running" and "finished, usage unknown" both serialise to it and come back
/// indistinguishable. A settled-unknown attempt would then look running again
/// after a reload, and a stray later report could release tokens that were held
/// precisely because nobody could account for them.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum Settlement {
    /// Admitted, not finished. The reservation stands.
    Running,
    /// Finished, and it reported what it used.
    Observed { tokens: u64 },
    /// Finished, and its usage never arrived. The reservation stands, because
    /// unknown is not zero.
    UsageUnknown,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Entry {
    attempt_id: String,
    reserved: u64,
    settlement: Settlement,
}

impl Entry {
    /// What this attempt counts against the ceiling.
    fn charge(&self) -> u64 {
        match self.settlement {
            Settlement::Observed { tokens } => tokens,
            // Running, or finished with usage unknown. Either way the tokens are
            // not known to be unspent, so the reservation stands.
            Settlement::Running | Settlement::UsageUnknown => self.reserved,
        }
    }

    fn is_unsettled(&self) -> bool {
        !matches!(self.settlement, Settlement::Observed { .. })
    }
}

/// Errors reaching the ledger. Refusals are not errors; see [`Refusal`].
#[derive(Debug)]
pub enum LedgerError {
    Io(PathBuf, io::Error),
    Corrupt(PathBuf, String),
}

impl std::fmt::Display for LedgerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LedgerError::Io(path, error) => write!(f, "{}: {error}", path.display()),
            LedgerError::Corrupt(path, why) => {
                write!(f, "{} is not a usable ledger: {why}", path.display())
            }
        }
    }
}

impl std::error::Error for LedgerError {}

impl Ledger {
    /// Opens the ledger for one run, creating the directory private to its owner.
    pub fn open(dir: &Path, run_id: &str) -> Result<Self, LedgerError> {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        if !dir.exists() {
            builder
                .create(dir)
                .map_err(|error| LedgerError::Io(dir.to_path_buf(), error))?;
        }
        let safe: String = run_id
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        Ok(Ledger {
            path: dir.join(format!("run-{safe}.json")),
            lock_path: dir.join(format!("run-{safe}.lock")),
        })
    }

    /// Reserves for one attempt, or explains why it cannot.
    ///
    /// Read, decide and write happen under one lock, so two processes admitting
    /// at the same instant cannot both see room that only one of them can have.
    pub fn admit(
        &self,
        attempt_id: &str,
        limits: &RunLimits,
    ) -> Result<Result<Admitted, Refusal>, LedgerError> {
        let _guard = self.lock()?;
        let mut state = self.read()?;

        // A retry under an id already admitted is the same attempt, not another
        // one. Re-reserving it would charge the run twice for one piece of work.
        if let Some(existing) = state.attempts.iter().find(|e| e.attempt_id == attempt_id) {
            let committed: u64 = state.attempts.iter().map(Entry::charge).sum();
            return Ok(Ok(Admitted {
                attempt_id: attempt_id.to_string(),
                reserved: existing.reserved,
                committed_after: committed,
                basis: CEILING_BASIS,
            }));
        }

        let started = state.attempts.len() as u32;
        if started >= limits.max_attempts {
            return Ok(Err(Refusal::AttemptsExhausted {
                started,
                max_attempts: limits.max_attempts,
            }));
        }

        let committed: u64 = state.attempts.iter().map(Entry::charge).sum();
        let unsettled: u64 = state
            .attempts
            .iter()
            .filter(|e| e.is_unsettled())
            .map(Entry::charge)
            .sum();
        let reserved = limits.attempt_reserve.max(1);

        if let Some(ceiling) = limits.token_ceiling
            && committed.saturating_add(reserved) > ceiling
        {
            return Ok(Err(Refusal::CeilingWouldBeExceeded {
                committed,
                unsettled,
                requested: reserved,
                ceiling,
            }));
        }

        state.attempts.push(Entry {
            attempt_id: attempt_id.to_string(),
            reserved,
            settlement: Settlement::Running,
        });
        let committed_after = state.attempts.iter().map(Entry::charge).sum();
        self.write(&state)?;

        Ok(Ok(Admitted {
            attempt_id: attempt_id.to_string(),
            reserved,
            committed_after,
            basis: CEILING_BASIS,
        }))
    }

    /// Records what an attempt actually used.
    ///
    /// `None` means the usage never arrived. The reservation is kept rather than
    /// released: an attempt that reported nothing may still have spent, and
    /// settling it to zero would let the run admit work it cannot account for.
    pub fn settle(&self, attempt_id: &str, observed: Option<u64>) -> Result<Settled, LedgerError> {
        let _guard = self.lock()?;
        let mut state = self.read()?;
        let mut found = false;
        for entry in &mut state.attempts {
            if entry.attempt_id == attempt_id {
                // Settled once, and only from Running. A repeated report -- or one
                // that arrives after the attempt was written off as unknown --
                // must not restate what it cost.
                if matches!(entry.settlement, Settlement::Running) {
                    entry.settlement = match observed {
                        Some(tokens) => Settlement::Observed { tokens },
                        None => Settlement::UsageUnknown,
                    };
                }
                found = true;
                break;
            }
        }
        if found {
            self.write(&state)?;
        }
        Ok(Settled {
            known: found,
            usage_known: observed.is_some(),
            committed: state.attempts.iter().map(Entry::charge).sum(),
            unsettled: state
                .attempts
                .iter()
                .filter(|e| e.is_unsettled())
                .map(Entry::charge)
                .sum(),
        })
    }

    /// The run's position, for reporting.
    pub fn summary(&self) -> Result<Summary, LedgerError> {
        let _guard = self.lock()?;
        let state = self.read()?;
        Ok(Summary {
            attempts: state.attempts.len() as u32,
            committed: state.attempts.iter().map(Entry::charge).sum(),
            unsettled: state
                .attempts
                .iter()
                .filter(|e| e.is_unsettled())
                .map(Entry::charge)
                .sum(),
            attempts_with_unknown_usage: state
                .attempts
                .iter()
                .filter(|e| matches!(e.settlement, Settlement::UsageUnknown))
                .count() as u32,
            basis: CEILING_BASIS,
        })
    }

    fn read(&self) -> Result<State, LedgerError> {
        match std::fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|error| LedgerError::Corrupt(self.path.clone(), error.to_string())),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(State::default()),
            Err(error) => Err(LedgerError::Io(self.path.clone(), error)),
        }
    }

    fn write(&self, state: &State) -> Result<(), LedgerError> {
        use std::io::Write;
        let temp = self.path.with_extension("partial");
        let body = serde_json::to_vec_pretty(state).expect("the ledger is serialisable");
        {
            let mut options = OpenOptions::new();
            options.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options
                .open(&temp)
                .map_err(|error| LedgerError::Io(temp.clone(), error))?;
            file.write_all(&body)
                .map_err(|error| LedgerError::Io(temp.clone(), error))?;
            // A reservation that is not on disk before the attempt starts is not
            // a reservation.
            file.sync_all()
                .map_err(|error| LedgerError::Io(temp.clone(), error))?;
        }
        std::fs::rename(&temp, &self.path).map_err(|error| LedgerError::Io(temp, error))
    }

    fn lock(&self) -> Result<LedgerGuard, LedgerError> {
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options
            .open(&self.lock_path)
            .map_err(|error| LedgerError::Io(self.lock_path.clone(), error))?;
        // Blocking: an admission that gave up because another process held the
        // lock would look like a refusal, which is a different thing entirely.
        file.lock()
            .map_err(|error| LedgerError::Io(self.lock_path.clone(), error))?;
        Ok(LedgerGuard { file })
    }
}

/// Holds the ledger lock for one read-modify-write.
struct LedgerGuard {
    file: File,
}

impl Drop for LedgerGuard {
    fn drop(&mut self) {
        // Released explicitly, for the same reason the worker slots are: the lock
        // belongs to the open file description, which a child forked by another
        // thread shares until it execs. Closing our descriptor alone would leave
        // the ledger locked and the next admission waiting on nothing.
        #[cfg(unix)]
        unsafe {
            libc::flock(std::os::fd::AsRawFd::as_raw_fd(&self.file), libc::LOCK_UN);
        }
    }
}

/// Outcome of settling an attempt.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct Settled {
    /// The attempt was in the ledger. False means it was never admitted here.
    pub known: bool,
    /// Real usage arrived. When false the reservation is kept, not released.
    pub usage_known: bool,
    pub committed: u64,
    pub unsettled: u64,
}

/// A run's position against its limits.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct Summary {
    pub attempts: u32,
    pub committed: u64,
    /// Part of `committed` held for attempts whose usage is not known.
    pub unsettled: u64,
    pub attempts_with_unknown_usage: u32,
    pub basis: &'static str,
}
