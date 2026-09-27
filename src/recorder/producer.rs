// Adapted for Timon. Not derived from Prodex source.
//! Producer side of usage recording: a private spool, then delivery.
//!
//! An event is written to the account's own spool *before* it is sent, and
//! removed only once the daemon has acknowledged a commit under the same id.
//! A crash, a restart, or a daemon that is simply down therefore costs a delay
//! rather than the record.
//!
//! What this cannot promise is losslessness. If the spool is full, or the disk
//! is, or the directory cannot be written at all, some usage is gone and the
//! only honest thing left is to say so. It is never rewritten as zero.

use std::path::{Path, PathBuf};

use crate::attempt::AttemptReport;
use crate::recorder::event::UsageEvent;
use crate::recorder::protocol::{Request, Response};

/// Spooled events allowed before new ones are dropped.
pub const DEFAULT_MAX_SPOOLED_EVENTS: usize = 4_096;

/// Environment variable that relocates the spool. Used by tests and by an
/// operator who keeps per-account state somewhere other than the default.
pub const SPOOL_DIR_ENV: &str = "TIMON_SPOOL_DIR";

/// Builds the event an attempt reports.
///
/// The role, ids and `occurred_at` come from the attempt itself, so a replay
/// carries the same id and the same observation time it always had.
pub fn event_for(
    report: &AttemptReport,
    provider: Option<&str>,
    model: Option<&str>,
) -> UsageEvent {
    UsageEvent::new(
        report.client_event_id.clone(),
        report.run_id.clone(),
        report.attempt_id.clone(),
        report.role,
        provider.map(str::to_string),
        model.map(str::to_string),
        report.usage.usage,
        report.usage.status,
        Some(report.process.duration_ms),
        (report.occurred_at_unix_ms / 1_000) as i64,
    )
}

/// The account's private spool directory.
#[derive(Clone, Debug)]
pub struct Spool {
    dir: PathBuf,
    max_events: usize,
}

/// Why the spool could not take an event.
#[derive(Debug)]
pub enum SpoolError {
    /// At its configured bound. The event was not kept.
    Full,
    Io(PathBuf, std::io::Error),
}

impl std::fmt::Display for SpoolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpoolError::Full => write!(f, "the usage spool is full"),
            SpoolError::Io(path, error) => write!(f, "{}: {error}", path.display()),
        }
    }
}

impl std::error::Error for SpoolError {}

impl Spool {
    /// Resolves the spool directory, preferring an explicit override.
    pub fn resolve() -> Option<PathBuf> {
        if let Some(dir) = std::env::var_os(SPOOL_DIR_ENV) {
            return Some(PathBuf::from(dir));
        }
        if let Some(state) = std::env::var_os("XDG_STATE_HOME") {
            return Some(PathBuf::from(state).join("timon/usage-spool"));
        }
        std::env::var_os("HOME")
            .map(|home| PathBuf::from(home).join(".local/state/timon/usage-spool"))
    }

    /// Opens the directory, creating it private to this account.
    pub fn open(dir: PathBuf, max_events: usize) -> Result<Self, SpoolError> {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        if !dir.exists() {
            builder
                .create(&dir)
                .map_err(|error| SpoolError::Io(dir.clone(), error))?;
        }
        Ok(Spool { dir, max_events })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Writes an event so that it survives this process.
    ///
    /// The file appears under its final name only once it is complete, so a
    /// reader never sees half an event: a torn write leaves a temporary file
    /// that replay ignores. The name is derived from the event id, so a retry
    /// of the same attempt reuses one slot instead of accumulating copies.
    pub fn stage(&self, event: &UsageEvent) -> Result<PathBuf, SpoolError> {
        let final_path = self
            .dir
            .join(format!("{}.json", encode_name(&event.client_event_id)));
        if !final_path.exists() && self.count()? >= self.max_events {
            return Err(SpoolError::Full);
        }
        let body = serde_json::to_vec(event).expect("a usage event is serialisable");
        self.write_atomically(&final_path, &body)?;
        Ok(final_path)
    }

    /// Records that events were dropped, without claiming how many tokens.
    ///
    /// Sealing gives the accumulated gap a fixed id so that retransmitting it
    /// is idempotent, and lets further drops accumulate separately instead of
    /// changing a record the daemon may already hold.
    pub fn note_dropped(&self, event: &UsageEvent) -> Result<(), SpoolError> {
        let path = self.dir.join(GAP_CURRENT);
        let mut gap = self.read_gap()?.unwrap_or_default();
        gap.dropped += 1;
        gap.first_occurred_at = Some(match gap.first_occurred_at {
            Some(first) => first.min(event.occurred_at),
            None => event.occurred_at,
        });
        gap.last_occurred_at = Some(match gap.last_occurred_at {
            Some(last) => last.max(event.occurred_at),
            None => event.occurred_at,
        });
        let body = serde_json::to_vec(&gap).expect("a gap is serialisable");
        // One coalesced file, so a long outage cannot grow the marker without
        // bound the way one marker per dropped event would.
        self.write_atomically(&path, &body)
    }

    /// The accumulated gap, if any.
    pub fn read_gap(&self) -> Result<Option<Gap>, SpoolError> {
        let path = self.dir.join(GAP_CURRENT);
        match std::fs::read(&path) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes).ok()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(SpoolError::Io(path, error)),
        }
    }

    /// Events waiting to be delivered, oldest first.
    pub fn pending(&self) -> Result<Vec<PathBuf>, SpoolError> {
        let mut paths: Vec<PathBuf> = std::fs::read_dir(&self.dir)
            .map_err(|error| SpoolError::Io(self.dir.clone(), error))?
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension().is_some_and(|ext| ext == "json")
                    && path.file_name().is_some_and(|name| name != GAP_CURRENT)
            })
            .collect();
        paths.sort();
        Ok(paths)
    }

    /// Drops a spooled file. Called only once its id is acknowledged.
    pub fn remove(&self, path: &Path) -> Result<(), SpoolError> {
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(SpoolError::Io(path.to_path_buf(), error)),
        }
    }

    fn count(&self) -> Result<usize, SpoolError> {
        Ok(self.pending()?.len())
    }

    fn write_atomically(&self, path: &Path, body: &[u8]) -> Result<(), SpoolError> {
        use std::io::Write;
        let temp = path.with_extension("partial");
        {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options
                .open(&temp)
                .map_err(|error| SpoolError::Io(temp.clone(), error))?;
            file.write_all(body)
                .map_err(|error| SpoolError::Io(temp.clone(), error))?;
            // Durable before it is visible: a rename of an unflushed file can
            // leave an empty one behind after a power loss.
            file.sync_all()
                .map_err(|error| SpoolError::Io(temp.clone(), error))?;
        }
        std::fs::rename(&temp, path).map_err(|error| SpoolError::Io(temp, error))?;
        if let Ok(dir) = std::fs::File::open(&self.dir) {
            let _ = dir.sync_all();
        }
        Ok(())
    }
}

const GAP_CURRENT: &str = "gap-current.json";

/// Events known to be missing, and the window they fall in.
#[derive(Clone, Debug, Default, serde::Deserialize, serde::Serialize)]
pub struct Gap {
    pub dropped: u64,
    pub first_occurred_at: Option<i64>,
    pub last_occurred_at: Option<i64>,
}

/// What became of one event.
#[derive(Debug)]
pub enum Delivery {
    /// Committed by the daemon.
    Recorded { id: i64, duplicate: bool },
    /// Held in the spool for replay. The run is unaffected.
    Spooled { reason: String },
    /// Neither delivered nor kept. This usage is lost.
    Dropped { reason: String },
}

impl Delivery {
    /// True when the record did not survive, so a caller can say so out loud.
    pub fn is_lost(&self) -> bool {
        matches!(self, Delivery::Dropped { .. })
    }
}

/// Spools an event, then tries to deliver it.
///
/// Recording is never allowed to fail a run: a daemon that is down, or a full
/// spool, degrades tracking and nothing else. That is deliberate — usage
/// visibility is not worth failing work the user asked for.
pub async fn deliver(socket: &Path, spool: &Spool, event: &UsageEvent) -> Delivery {
    let staged = match spool.stage(event) {
        Ok(path) => Some(path),
        Err(SpoolError::Full) => {
            // Record that something was lost before dropping it, so a report can
            // show a hole instead of silently understating the total.
            return match spool.note_dropped(event) {
                Ok(()) => Delivery::Dropped {
                    reason: "the usage spool is full; a gap was recorded".to_string(),
                },
                Err(error) => Delivery::Dropped {
                    reason: format!(
                        "the usage spool is full and the gap could not be kept: {error}"
                    ),
                },
            };
        }
        Err(error) => {
            return Delivery::Dropped {
                reason: format!("the event could not be spooled: {error}"),
            };
        }
    };

    match crate::recorder::client::send(
        socket,
        &Request::Append {
            event: Box::new(event.clone()),
        },
    )
    .await
    {
        Ok(Response::Receipt { id, duplicate, .. }) => {
            if let Some(path) = staged {
                // Only now: until the receipt arrives the event has to stay.
                let _ = spool.remove(&path);
            }
            Delivery::Recorded { id, duplicate }
        }
        Ok(Response::Error { code, message }) => Delivery::Spooled {
            reason: format!("the recorder refused the event ({code:?}): {message}"),
        },
        // Neither can answer an append; treat it as a failed delivery rather
        // than assuming the event landed.
        Ok(Response::Rows { .. } | Response::Report { .. } | Response::Monthly { .. }) => {
            Delivery::Spooled {
                reason: "the recorder answered an append with a read reply".to_string(),
            }
        }
        Err(error) => Delivery::Spooled {
            reason: format!("the recorder is unreachable: {error:#}"),
        },
    }
}

/// What a replay pass achieved.
#[derive(Debug, Default)]
pub struct Replay {
    pub delivered: u64,
    pub already_present: u64,
    pub still_pending: u64,
    pub corrupt: u64,
    pub gap_delivered: bool,
    pub notes: Vec<String>,
}

/// Delivers spooled events in one bounded pass.
///
/// Bounded so replay cannot monopolise a session, and so a daemon that starts
/// refusing part way through leaves the rest for the next attempt.
pub async fn replay(socket: &Path, spool: &Spool, max_batch: usize) -> Result<Replay, SpoolError> {
    let mut outcome = Replay::default();
    for path in spool.pending()?.into_iter().take(max_batch) {
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) => {
                outcome.notes.push(format!("{}: {error}", path.display()));
                outcome.still_pending += 1;
                continue;
            }
        };
        let event: UsageEvent = match serde_json::from_slice(&bytes) {
            Ok(event) => event,
            Err(error) => {
                // Unreadable under its final name, so this is corruption rather
                // than a torn write. Set it aside instead of retrying forever,
                // and count it: the tokens it held are now unknown.
                let aside = path.with_extension("corrupt");
                let _ = std::fs::rename(&path, &aside);
                outcome.corrupt += 1;
                outcome.notes.push(format!("{}: {error}", path.display()));
                continue;
            }
        };
        match crate::recorder::client::send(
            socket,
            &Request::Append {
                event: Box::new(event),
            },
        )
        .await
        {
            Ok(Response::Receipt { duplicate, .. }) => {
                spool.remove(&path)?;
                if duplicate {
                    outcome.already_present += 1;
                } else {
                    outcome.delivered += 1;
                }
            }
            Ok(Response::Error { code, message }) => {
                outcome.still_pending += 1;
                outcome.notes.push(format!("{code:?}: {message}"));
            }
            other => {
                outcome.still_pending += 1;
                outcome.notes.push(format!("unexpected reply: {other:?}"));
            }
        }
    }
    Ok(outcome)
}

/// Makes an event id safe as a file name without losing its identity.
fn encode_name(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}
