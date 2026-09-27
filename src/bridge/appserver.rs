// Adapted for Timon. Not derived from Prodex source.
//! A pass-through proxy for the Codex app-server protocol that records usage.

use serde::{Deserialize, Serialize};

use crate::usage::{TokenCount, TokenUsage, UsageStatus};

/// The notification that carries token counts.
pub const USAGE_METHOD: &str = "thread/tokenUsage/updated";

/// One breakdown from the protocol, as `TokenUsageBreakdown`.
///
/// Absent fields are `Unknown` rather than zero, for the same reason the rest of
/// the project insists on it: a missing count is not a count of nothing, and
/// folding one in as zero makes a total read as complete when part of it is not.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Breakdown {
    pub input_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub reasoning_output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    /// Present in the protocol, not carried into Timon's envelope: it is a write
    /// to a cache rather than tokens spent answering, and adding it to a total
    /// would overstate the turn.
    #[serde(default)]
    pub cache_write_input_tokens: Option<u64>,
}

impl Breakdown {
    /// Converts to Timon's envelope.
    pub fn to_usage(self) -> TokenUsage {
        TokenUsage {
            input: count(self.input_tokens),
            cached_input: count(self.cached_input_tokens),
            output: count(self.output_tokens),
            reasoning_output: count(self.reasoning_output_tokens),
        }
    }

    /// True when the breakdown reported nothing at all.
    pub fn is_empty(&self) -> bool {
        self.input_tokens.is_none()
            && self.cached_input_tokens.is_none()
            && self.output_tokens.is_none()
            && self.reasoning_output_tokens.is_none()
            && self.total_tokens.is_none()
    }
}

fn count(value: Option<u64>) -> TokenCount {
    match value {
        Some(value) => TokenCount::Known(value),
        None => TokenCount::Unknown,
    }
}

/// `ThreadTokenUsage`: the turn that just finished, and the thread so far.
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub struct ThreadUsage {
    pub last: Breakdown,
    pub total: Breakdown,
}

/// `ThreadTokenUsageUpdatedNotification`.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageNotification {
    pub thread_id: String,
    pub turn_id: String,
    pub token_usage: ThreadUsage,
}

/// What one observed notification means for the recorder.
#[derive(Clone, Debug, Serialize)]
pub struct Observation {
    pub thread_id: String,
    pub turn_id: String,
    /// The turn's own usage, which is what gets recorded.
    pub usage: TokenUsage,
    pub status: UsageStatus,
    /// The thread total the server reported alongside it. Kept so a reader can
    /// see the figure this was checked against.
    pub thread_total: Option<u64>,
    /// Set when the running sum of per-turn `last` figures has drifted from the
    /// server's own `total`. Not an error and never suppresses the record: the
    /// protocol is free to compact a thread or start counting differently, and a
    /// silent disagreement is worth surfacing rather than resolving by guess.
    pub disagrees_with_total: bool,
}

/// A stable event id for one turn of one thread.
///
/// Derived rather than generated so that a client which replays a notification,
/// or a session resumed after a restart, collapses onto the row already stored
/// instead of double counting. Matches how supervised attempts derive theirs.
pub fn event_id(thread_id: &str, turn_id: &str) -> String {
    format!("appserver:{thread_id}:{turn_id}")
}

/// Reads the protocol stream and decides what to record.
///
/// Deliberately tolerant. This sits in the path of somebody's editor, so an
/// unparsable line, an unknown method, or a notification whose shape has changed
/// must never be fatal and must never alter the traffic — the worst outcome
/// allowed here is that a turn goes unrecorded.
#[derive(Debug, Default)]
pub struct Observer {
    /// Running sum of the per-turn figures recorded so far, per thread, used
    /// only to notice disagreement with the server's own total.
    seen: std::collections::HashMap<String, u64>,
    /// Turns already observed, so a replayed notification is not reported twice
    /// by this process. The recorder deduplicates as well; this saves the round
    /// trip and keeps the spool from filling with known duplicates.
    recorded: std::collections::HashSet<String>,
    /// Lines that looked like the usage notification but could not be read.
    pub malformed: u64,
}

impl Observer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Inspects one line of the server's output.
    ///
    /// Returns `Some` only for a usage notification not already seen. Every other
    /// line — every other method, every unparsable byte sequence — returns
    /// `None`, and the caller forwards it regardless.
    pub fn observe(&mut self, line: &str) -> Option<Observation> {
        let trimmed = line.trim();
        if !trimmed.starts_with('{') || !trimmed.contains(USAGE_METHOD) {
            return None;
        }
        let message: serde_json::Value = serde_json::from_str(trimmed).ok()?;
        if message.get("method").and_then(|m| m.as_str()) != Some(USAGE_METHOD) {
            return None;
        }
        let params = message.get("params")?;
        let Ok(note) = serde_json::from_value::<UsageNotification>(params.clone()) else {
            // Shaped like the notification but not readable as one: the protocol
            // is experimental and may have changed under us. Counted so an
            // operator can see it, rather than silently ignored.
            self.malformed += 1;
            return None;
        };

        let key = event_id(&note.thread_id, &note.turn_id);
        if !self.recorded.insert(key) {
            return None;
        }

        let last = note.token_usage.last;
        let usage = last.to_usage();
        // A breakdown reporting nothing is recorded as unknown usage rather than
        // dropped: the turn happened, and its cost being unreported is a fact
        // worth keeping.
        let status = if last.is_empty() {
            UsageStatus::Unknown
        } else if usage.total().is_known() {
            UsageStatus::Complete
        } else {
            UsageStatus::Partial
        };

        let running = self.seen.entry(note.thread_id.clone()).or_insert(0);
        *running = running.saturating_add(last.total_tokens.unwrap_or(0));
        let disagrees = match note.token_usage.total.total_tokens {
            Some(total) => *running != total,
            None => false,
        };

        Some(Observation {
            thread_id: note.thread_id,
            turn_id: note.turn_id,
            usage,
            status,
            thread_total: note.token_usage.total.total_tokens,
            disagrees_with_total: disagrees,
        })
    }

    /// Turns already observed by this process.
    pub fn turns_seen(&self) -> usize {
        self.recorded.len()
    }
}

// ---------------------------------------------------------------------------
// The proxy
// ---------------------------------------------------------------------------

use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::attempt::Role;
use crate::recorder::event::UsageEvent;
use crate::recorder::producer::{Delivery, Spool, deliver};

/// How the bridge was configured.
pub struct Config {
    /// The app-server to run, and its arguments.
    pub command: Vec<std::ffi::OsString>,
    /// The recorder socket. `None` records nothing, which is useful for checking
    /// that the pass-through is transparent without a daemon running.
    pub socket: Option<PathBuf>,
    /// This account's spool, so a recorder outage costs a delay rather than the
    /// record.
    pub spool: Option<Spool>,
    /// Model name to attribute, when the caller knows it. The protocol's usage
    /// notification does not carry one.
    pub model: Option<String>,
}

/// What the bridge did, reported when the child exits.
#[derive(Debug, Default, Serialize)]
pub struct Report {
    /// Bytes forwarded in each direction, so transparency is measurable rather
    /// than asserted.
    pub bytes_client_to_server: u64,
    pub bytes_server_to_client: u64,
    pub turns_observed: u64,
    pub events_recorded: u64,
    pub events_spooled: u64,
    /// Notifications that looked like usage but could not be read, which is how
    /// a protocol change shows up here.
    pub malformed_usage_notifications: u64,
    /// Turns whose per-turn figures did not sum to the server's thread total.
    pub disagreed_with_thread_total: u64,
    pub exit_code: Option<i32>,
}

/// Runs the app-server as a child and proxies it, recording usage on the way.
///
/// Transparency is the contract: every byte the client sends reaches the server
/// and every byte the server sends reaches the client, unaltered and in order.
/// That is why the forwarding write happens *before* the line is parsed — the
/// observer sees a copy of what has already been delivered, so nothing it does,
/// including panicking on a shape it has never seen, can change what the client
/// receives.
pub async fn run(config: Config) -> std::io::Result<Report> {
    let mut command = tokio::process::Command::new(&config.command[0]);
    command
        .args(&config.command[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // Inherited: the app-server writes diagnostics there and a client may be
        // relying on seeing them.
        .stderr(Stdio::inherit());
    let mut child = command.spawn()?;

    let mut to_child = child.stdin.take().expect("stdin was piped");
    let mut from_child = child.stdout.take().expect("stdout was piped");

    // Observations go to a separate task so a slow or unreachable recorder can
    // never hold up the protocol.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Observation>();

    let recorder = {
        let socket = config.socket.clone();
        let spool = config.spool;
        let model = config.model.clone();
        tokio::spawn(async move {
            let mut recorded = 0u64;
            let mut spooled = 0u64;
            while let Some(observation) = rx.recv().await {
                let (Some(socket), Some(spool)) = (socket.as_ref(), spool.as_ref()) else {
                    continue;
                };
                let event = UsageEvent::new(
                    event_id(&observation.thread_id, &observation.turn_id),
                    observation.thread_id.clone(),
                    observation.turn_id.clone(),
                    // An interactive session is the person's own turn, not a
                    // delegated one.
                    Role::Lead,
                    Some("openai".to_string()),
                    model.clone(),
                    observation.usage,
                    observation.status,
                    None,
                    now_secs(),
                );
                match deliver(socket, spool, &event).await {
                    Delivery::Recorded { .. } => recorded += 1,
                    _ => spooled += 1,
                }
            }
            (recorded, spooled)
        })
    };

    // Counted through a shared cell rather than returned, because this task is
    // abandoned rather than awaited when the child exits first: a client may hold
    // its end of stdin open indefinitely, and a read that never returns must not
    // keep the bridge alive after the thing it was proxying has gone.
    let sent = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let client_to_server = {
        let sent = std::sync::Arc::clone(&sent);
        tokio::spawn(async move {
            let mut stdin = tokio::io::stdin();
            let mut buffer = [0u8; 16 * 1024];
            loop {
                match stdin.read(&mut buffer).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if to_child.write_all(&buffer[..n]).await.is_err() {
                            break;
                        }
                        if to_child.flush().await.is_err() {
                            break;
                        }
                        sent.fetch_add(n as u64, std::sync::atomic::Ordering::Relaxed);
                    }
                }
            }
            // Closing the child's stdin is how the app-server learns the client
            // has gone, so it must happen rather than waiting for process
            // teardown.
            drop(to_child);
        })
    };

    let mut stdout = tokio::io::stdout();
    let mut observer = Observer::new();
    let mut buffer = [0u8; 16 * 1024];
    let mut line = Vec::new();
    let mut report = Report::default();

    loop {
        let read = match from_child.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        let chunk = &buffer[..read];
        // Forwarded first, and flushed, so the client is never waiting on
        // anything this function does next.
        stdout.write_all(chunk).await?;
        stdout.flush().await?;
        report.bytes_server_to_client += read as u64;

        // Observation works on a copy. Lines are split only to find the usage
        // notification; nothing here is ever written back to the client.
        for &byte in chunk {
            if byte == b'\n' {
                if let Ok(text) = std::str::from_utf8(&line)
                    && let Some(observation) = observer.observe(text)
                {
                    report.turns_observed += 1;
                    if observation.disagrees_with_total {
                        report.disagreed_with_thread_total += 1;
                    }
                    // Send failure means the recorder task is gone; the session
                    // continues regardless.
                    let _ = tx.send(observation);
                }
                line.clear();
            } else {
                line.push(byte);
            }
        }
    }

    drop(tx);
    let status = child.wait().await?;
    report.exit_code = status.code();
    // Abandoned deliberately, not awaited. Reaching here means the child's output
    // is at end of file and the child has been reaped, so there is nothing left to
    // forward; stdin, on the other hand, may stay open for as long as the client
    // lives. Awaiting this hung the bridge after a short-lived child exited --
    // `codex --version` through the wrapper never returned -- which is exactly
    // the health check an editor is likely to run first.
    client_to_server.abort();
    report.bytes_client_to_server = sent.load(std::sync::atomic::Ordering::Relaxed);
    report.malformed_usage_notifications = observer.malformed;
    if let Ok((recorded, spooled)) = recorder.await {
        report.events_recorded = recorded;
        report.events_spooled = spooled;
    }
    Ok(report)
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

/// True when `path` looks like a shared app-server daemon socket.
///
/// Worth checking before advertising this: a *shared* daemon serves several
/// accounts from one process, so the uid the recorder would see is the daemon's
/// rather than the person's, and a cross-account session mix-up has been
/// reported upstream (openai/codex#19590). The bridge is only honest when each
/// account runs its own child.
pub fn looks_like_shared_daemon(command: &[std::ffi::OsString]) -> bool {
    command.iter().any(|arg| {
        let arg = arg.to_string_lossy();
        arg == "daemon" || arg == "proxy" || arg.contains("--code-mode-host")
    })
}

pub fn spool_hint() -> Option<PathBuf> {
    Spool::resolve()
}

/// Where the recorder socket usually is, for the CLI's default.
pub fn default_socket() -> &'static Path {
    Path::new("/run/timon-usage/usage.sock")
}
