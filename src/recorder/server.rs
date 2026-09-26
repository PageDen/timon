// Adapted for Timon. Not derived from Prodex source.
//! The recorder daemon.
//!
//! Identity comes from the kernel. Every connection's uid is read from the
//! socket with `SO_PEERCRED`, and that uid decides both what the row is
//! attributed to and what the caller may read. Nothing in the payload can
//! change either, so an ordinary client cannot record usage as another account
//! or read an account that is not its own.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;

use crate::recorder::db::{AppendError, Scope, Store, totals};
use crate::recorder::event::{MAX_REQUEST_BYTES, UsageEvent};
use crate::recorder::protocol::{
    DEFAULT_QUERY_LIMIT, ErrorCode, MAX_QUERY_LIMIT, Request, Response,
};

/// How the daemon is configured.
pub struct Config {
    pub socket: PathBuf,
    pub database: PathBuf,
    /// Principals allowed to read every account. Server-side policy: an
    /// administrator is named here by the operator, never self-declared by a
    /// client and never inferred from the caller's own group membership.
    pub admin_uids: Vec<u32>,
    /// Mode for the socket file. Access is granted by group membership, so the
    /// group must be set by the operator (or systemd) on the parent directory.
    pub socket_mode: u32,
    pub max_requests_per_second: u32,
}

impl Config {
    pub fn new(socket: PathBuf, database: PathBuf) -> Self {
        Config {
            socket,
            database,
            admin_uids: Vec::new(),
            socket_mode: 0o660,
            max_requests_per_second: 50,
        }
    }
}

/// Serves until `shutdown` completes.
pub async fn serve(config: Config, shutdown: impl Future<Output = ()>) -> Result<()> {
    let store = Store::open(&config.database)
        .with_context(|| format!("opening {}", config.database.display()))?;
    let store = Arc::new(Mutex::new(store));

    // A stale socket from a previous run would make bind fail. Removing it is
    // safe only because the daemon owns this path; the directory it lives in is
    // operator-owned so no client can put something else here for us to unlink.
    if config.socket.exists() {
        std::fs::remove_file(&config.socket)
            .with_context(|| format!("removing stale socket {}", config.socket.display()))?;
    }
    let listener = UnixListener::bind(&config.socket)
        .with_context(|| format!("binding {}", config.socket.display()))?;
    set_socket_mode(&config.socket, config.socket_mode)?;

    let config = Arc::new(config);
    let limiter = Arc::new(Mutex::new(RateLimiter::default()));
    let mut shutdown = Box::pin(shutdown);

    loop {
        tokio::select! {
            () = &mut shutdown => break,
            accepted = listener.accept() => {
                let (stream, _) = match accepted {
                    Ok(pair) => pair,
                    // One failed accept must not take the service away from
                    // everybody else.
                    Err(error) => {
                        eprintln!("timon-usage: accept failed: {error}");
                        continue;
                    }
                };
                let store = Arc::clone(&store);
                let config = Arc::clone(&config);
                let limiter = Arc::clone(&limiter);
                tokio::spawn(async move {
                    if let Err(error) = serve_connection(stream, store, config, limiter).await {
                        eprintln!("timon-usage: connection ended: {error}");
                    }
                });
            }
        }
    }

    let _ = std::fs::remove_file(&config.socket);
    Ok(())
}

async fn serve_connection(
    stream: UnixStream,
    store: Arc<Mutex<Store>>,
    config: Arc<Config>,
    limiter: Arc<Mutex<RateLimiter>>,
) -> Result<()> {
    // The kernel's answer, not the client's. This is the whole trust boundary.
    let credentials = stream.peer_cred().context("reading peer credentials")?;
    let peer_uid = credentials.uid();
    let peer_username = username_for(peer_uid);

    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half.take(MAX_REQUEST_BYTES as u64 + 1));
    let mut line = String::new();

    loop {
        line.clear();
        let read = reader.read_line(&mut line).await?;
        if read == 0 {
            return Ok(());
        }
        if read > MAX_REQUEST_BYTES {
            reply(
                &mut write_half,
                &Response::Error {
                    code: ErrorCode::TooLarge,
                    message: format!("request exceeds {MAX_REQUEST_BYTES} bytes"),
                },
            )
            .await?;
            return Ok(());
        }
        // Restore the budget for the next request on this connection.
        reader.get_mut().set_limit(MAX_REQUEST_BYTES as u64 + 1);

        if !limiter
            .lock()
            .await
            .allow(peer_uid, config.max_requests_per_second)
        {
            reply(
                &mut write_half,
                &Response::Error {
                    code: ErrorCode::RateLimited,
                    message: "too many requests".to_string(),
                },
            )
            .await?;
            continue;
        }

        let response = match serde_json::from_str::<Request>(line.trim_end()) {
            Ok(request) => {
                handle(request, peer_uid, peer_username.as_deref(), &store, &config).await
            }
            Err(error) => Response::Error {
                code: ErrorCode::Malformed,
                message: error.to_string(),
            },
        };
        reply(&mut write_half, &response).await?;
    }
}

async fn handle(
    request: Request,
    peer_uid: u32,
    peer_username: Option<&str>,
    store: &Arc<Mutex<Store>>,
    config: &Config,
) -> Response {
    let is_admin = config.admin_uids.contains(&peer_uid);
    match request {
        Request::Append { event } => append(*event, peer_uid, peer_username, store).await,
        Request::Query {
            since,
            until,
            only_uid,
            limit,
        } => {
            let scope = match only_uid {
                None if is_admin => Scope::Admin { only_uid: None },
                None => Scope::Own(peer_uid),
                Some(uid) if is_admin => Scope::Admin {
                    only_uid: Some(uid),
                },
                // Refused outright. Quietly narrowing this to the caller's own
                // rows would hand back data under a heading the caller did not
                // ask for, which reads like another account's usage.
                Some(uid) if uid == peer_uid => Scope::Own(peer_uid),
                Some(_) => {
                    return Response::Error {
                        code: ErrorCode::Forbidden,
                        message: "reading another principal requires administrator policy"
                            .to_string(),
                    };
                }
            };
            let limit = limit.unwrap_or(DEFAULT_QUERY_LIMIT).min(MAX_QUERY_LIMIT);
            let guard = store.lock().await;
            match guard.query(scope, since, until, limit) {
                Ok(rows) => {
                    let totals = totals(&rows);
                    let scope_uid = match scope {
                        Scope::Own(uid) => Some(uid),
                        Scope::Admin { only_uid } => only_uid,
                    };
                    Response::Rows {
                        rows,
                        totals,
                        scope_uid,
                    }
                }
                Err(error) => Response::Error {
                    code: ErrorCode::Storage,
                    message: error.to_string(),
                },
            }
        }
    }
}

async fn append(
    event: UsageEvent,
    peer_uid: u32,
    peer_username: Option<&str>,
    store: &Arc<Mutex<Store>>,
) -> Response {
    if let Err(error) = event.validate() {
        return Response::Error {
            code: ErrorCode::InvalidEvent,
            message: error.to_string(),
        };
    }
    let identity_claim_ignored = event.carried_identity_claim();
    let received_at = now_secs();

    let mut guard = store.lock().await;
    match guard.append(peer_uid, peer_username, &event, received_at) {
        // Reached only after the transaction committed, so a receipt always
        // means the row is durable.
        Ok(receipt) => Response::Receipt {
            id: receipt.id,
            duplicate: receipt.duplicate,
            identity_claim_ignored,
        },
        Err(AppendError::Conflict { existing_id }) => Response::Error {
            code: ErrorCode::Conflict,
            message: format!("event id already stored as row {existing_id} with other content"),
        },
        Err(AppendError::UncorrectableTarget { corrects }) => Response::Error {
            code: ErrorCode::Forbidden,
            message: format!("row {corrects} is not yours to correct"),
        },
        Err(AppendError::Sql(error)) => Response::Error {
            code: ErrorCode::Storage,
            message: error.to_string(),
        },
    }
}

async fn reply(
    write_half: &mut tokio::net::unix::OwnedWriteHalf,
    response: &Response,
) -> Result<()> {
    let mut line = serde_json::to_string(response)?;
    line.push('\n');
    write_half.write_all(line.as_bytes()).await?;
    write_half.flush().await?;
    Ok(())
}

/// Per-principal request budget, refilled every second.
#[derive(Default)]
struct RateLimiter {
    seen: HashMap<u32, (Instant, u32)>,
}

impl RateLimiter {
    fn allow(&mut self, uid: u32, per_second: u32) -> bool {
        let now = Instant::now();
        let entry = self.seen.entry(uid).or_insert((now, 0));
        if now.duration_since(entry.0) >= Duration::from_secs(1) {
            *entry = (now, 0);
        }
        if entry.1 >= per_second {
            return false;
        }
        entry.1 += 1;
        true
    }
}

fn set_socket_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let permissions = std::fs::Permissions::from_mode(mode);
    std::fs::set_permissions(path, permissions)
        .with_context(|| format!("setting mode {mode:o} on {}", path.display()))
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

/// Resolves a uid to a name for display only.
///
/// The number stays the authority: renaming an account does not make it a
/// different principal, and a recycled uid would inherit the old rows, which is
/// why the deployment must not recycle one while records are retained.
fn username_for(uid: u32) -> Option<String> {
    // SAFETY: getpwuid returns a pointer into library-owned storage that stays
    // valid until the next call from this thread; the name is copied out at once.
    unsafe {
        let entry = libc::getpwuid(uid);
        if entry.is_null() {
            return None;
        }
        let name = (*entry).pw_name;
        if name.is_null() {
            return None;
        }
        std::ffi::CStr::from_ptr(name)
            .to_str()
            .ok()
            .map(str::to_string)
    }
}
