// Adapted for Timon. Not derived from Prodex source.
//! The forwarding proxy: inject a pooled credential, relay the answer.
//!
//! Synchronous, one thread per connection, in the same style as
//! [`crate::research::fetch`]. A handful of accounts on one host does not need an
//! async runtime, and streaming is far easier to get right when a read and the
//! write that follows it are adjacent lines rather than separate tasks.
//!
//! # What must not be broken
//!
//! **Streaming.** The upstream answers with `text/event-stream`, and a client
//! shows tokens as they arrive. Buffering a response to inspect it would turn a
//! responsive session into a long pause and a wall of text. Every read is
//! written and flushed before the next read is attempted. The routability probe
//! that established this design failed with `stream disconnected before
//! completion` precisely because a plain JSON reply is not a stream, which was a
//! useful way to learn it early.
//!
//! **Identity.** The caller's uid comes from the kernel, and a request whose
//! caller cannot be identified is refused. Never attributed to a default.
//!
//! **Silence about content.** This process sees every prompt and every response.
//! Nothing here writes a body, a header value, or a credential anywhere. The
//! report counts bytes and nothing else.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;

use crate::broker::grant::{GRANT_HEADER, Grants};
use crate::broker::health;
use crate::broker::identity::peer_uid;
use crate::broker::policy::{self, Decision, ModelPolicy};
use crate::broker::select::{self, NoAccount, Pool};
use crate::broker::store::{Account, Credential};

/// Where subscription-mode Codex sends its traffic when nothing redirects it.
///
/// `chatgpt.com`, not `chat.openai.com`. The other host answers, and answers with
/// a 401 saying the login "did not make it to this service", which reads like a
/// credential problem and is not one.
pub const DEFAULT_UPSTREAM: &str = "https://chatgpt.com/backend-api/codex";

/// Longest request line or header line accepted, so a hostile client cannot
/// exhaust memory before it has authenticated to anything.
const MAX_LINE: usize = 16 * 1024;

/// Most header lines accepted in one request.
const MAX_HEADERS: usize = 200;

/// Largest request body accepted. A turn's prompt measured 48,904 bytes in
/// testing; this leaves room for a much larger workspace context without being
/// unbounded.
pub const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;

/// Copy buffer. Small enough that a slow trickle of tokens is forwarded promptly
/// rather than waiting to fill.
const RELAY_CHUNK: usize = 8 * 1024;

/// Headers the broker supplies and a client must not be able to dictate.
///
/// Stripped from whatever arrives rather than merged: a caller that sent its own
/// `authorization` would otherwise choose which account paid for its request.
const STRIPPED: [&str; 5] = [
    "authorization",
    "chatgpt-account-id",
    "openai-organization",
    "openai-project",
    // A grant is between the caller and this broker. The provider has no use
    // for it, and forwarding it would put a live credential-shaped secret into
    // somebody else's logs.
    GRANT_HEADER,
];

/// Running totals, reported when the listener stops.
#[derive(Debug, Default, Serialize)]
pub struct Counters {
    pub connections: AtomicU64,
    pub requests_forwarded: AtomicU64,
    pub requests_refused_unidentified: AtomicU64,
    /// Requests refused because the pool had nothing that could serve them.
    pub requests_refused_no_account: AtomicU64,
    /// Requests served by a second account after the first could not serve them.
    pub rotations: AtomicU64,
    pub refreshes: AtomicU64,
    /// Renewals triggered by the provider refusing an unexpired token, which is
    /// how a subscription change presents.
    pub refreshes_after_refusal: AtomicU64,
    /// Requests whose model was replaced by policy. Announced on the response as
    /// well as counted, because a substitution nobody can see is the failure.
    pub model_substitutions: AtomicU64,
    pub grants_issued: AtomicU64,
    /// Requests that presented a grant the broker would not honour.
    pub grants_refused: AtomicU64,
    pub quota_reads: AtomicU64,
    pub bytes_to_upstream: AtomicU64,
    pub bytes_to_client: AtomicU64,
}

/// One parsed request, with its body held whole.
///
/// `Debug` is written by hand rather than derived, and deliberately. A derived
/// implementation would print the body — which is somebody's prompt — the moment
/// anything formatted a request into an error, a panic message or a log line.
/// This one reports the shape and not the content: header *names* without their
/// values, and the body's length without its bytes.
///
/// The body is read in full rather than streamed upstream because it is a bounded
/// JSON document and because the upstream needs a `content-length` it can trust.
/// Responses are the opposite case and are never buffered.
pub struct Request {
    pub method: String,
    pub target: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Request")
            .field("method", &self.method)
            .field("target", &self.target)
            .field(
                "header_names",
                &self.headers.iter().map(|(n, _)| n).collect::<Vec<_>>(),
            )
            .field("body_bytes", &self.body.len())
            .finish()
    }
}

/// Why a request could not be served.
#[derive(Debug)]
pub enum ServeError {
    /// Nothing about the request was readable. The connection is closed.
    Malformed(&'static str),
    TooLarge(&'static str),
    Io(std::io::Error),
}

impl std::fmt::Display for ServeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ServeError::Malformed(what) => write!(f, "malformed request: {what}"),
            ServeError::TooLarge(what) => write!(f, "{what} exceeds the accepted size"),
            ServeError::Io(error) => write!(f, "{error}"),
        }
    }
}

impl From<std::io::Error> for ServeError {
    fn from(error: std::io::Error) -> Self {
        ServeError::Io(error)
    }
}

/// Reads one HTTP/1.1 request.
///
/// Returns `Ok(None)` at a clean end of stream, which is how a client closing an
/// idle keep-alive connection presents and is not an error.
pub fn read_request<R: BufRead>(reader: &mut R) -> Result<Option<Request>, ServeError> {
    let Some(start) = read_line(reader)? else {
        return Ok(None);
    };
    if start.trim().is_empty() {
        return Ok(None);
    }
    let mut parts = start.trim_end().split(' ');
    let method = parts
        .next()
        .ok_or(ServeError::Malformed("no method"))?
        .to_string();
    let target = parts
        .next()
        .ok_or(ServeError::Malformed("no request target"))?
        .to_string();

    let mut headers = Vec::new();
    loop {
        let Some(line) = read_line(reader)? else {
            return Err(ServeError::Malformed("headers ended without a blank line"));
        };
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        if headers.len() >= MAX_HEADERS {
            return Err(ServeError::TooLarge("header count"));
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(ServeError::Malformed("header without a colon"));
        };
        headers.push((name.trim().to_lowercase(), value.trim().to_string()));
    }

    // Chunked request bodies are refused rather than mis-parsed. Codex sends a
    // content-length; guessing at a framing this has never seen would be the kind
    // of silent wrongness that is hard to notice.
    if headers
        .iter()
        .any(|(n, v)| n == "transfer-encoding" && v.to_lowercase().contains("chunked"))
    {
        return Err(ServeError::Malformed(
            "chunked request bodies are not accepted",
        ));
    }

    let length: usize = headers
        .iter()
        .find(|(n, _)| n == "content-length")
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    if length > MAX_BODY_BYTES {
        return Err(ServeError::TooLarge("request body"));
    }
    let mut body = vec![0u8; length];
    if length > 0 {
        reader.read_exact(&mut body)?;
    }

    Ok(Some(Request {
        method,
        target,
        headers,
        body,
    }))
}

fn read_line<R: BufRead>(reader: &mut R) -> Result<Option<String>, ServeError> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match reader.read(&mut byte)? {
            0 if line.is_empty() => return Ok(None),
            0 => break,
            _ => {
                line.push(byte[0]);
                if byte[0] == b'\n' {
                    break;
                }
                if line.len() > MAX_LINE {
                    return Err(ServeError::TooLarge("header line"));
                }
            }
        }
    }
    Ok(Some(String::from_utf8_lossy(&line).into_owned()))
}

/// Builds the upstream request, with the caller's own auth headers removed and
/// the pooled account's substituted.
///
/// `host` replaces whatever the client sent, because the request is going
/// somewhere else than the client addressed it.
pub fn upstream_request(
    request: &Request,
    path_prefix: &str,
    host: &str,
    bearer: &str,
    account_id: Option<&str>,
) -> Vec<u8> {
    let target = join_path(path_prefix, &request.target);
    let mut out = format!("{} {} HTTP/1.1\r\n", request.method, target);
    out.push_str(&format!("host: {host}\r\n"));
    out.push_str(&format!("authorization: Bearer {bearer}\r\n"));
    if let Some(id) = account_id {
        out.push_str(&format!("chatgpt-account-id: {id}\r\n"));
    }
    for (name, value) in &request.headers {
        if STRIPPED.contains(&name.as_str()) || name == "host" || name == "content-length" {
            continue;
        }
        // Connection handling is the proxy's business, not the client's.
        if name == "connection" || name == "proxy-connection" || name == "keep-alive" {
            continue;
        }
        out.push_str(&format!("{name}: {value}\r\n"));
    }
    out.push_str(&format!("content-length: {}\r\n", request.body.len()));
    out.push_str("connection: close\r\n\r\n");
    let mut bytes = out.into_bytes();
    bytes.extend_from_slice(&request.body);
    bytes
}

/// Joins the upstream's base path with the path the client asked for.
fn join_path(prefix: &str, target: &str) -> String {
    let prefix = prefix.trim_end_matches('/');
    if prefix.is_empty() {
        return target.to_string();
    }
    if target.starts_with('/') {
        format!("{prefix}{target}")
    } else {
        format!("{prefix}/{target}")
    }
}

/// A refusal the client can understand, for a request that will not be forwarded.
pub fn refusal(status: u16, reason: &str, detail: &str) -> Vec<u8> {
    let body = serde_json::json!({
        "error": {
            "message": detail,
            "type": "timon_broker_refused",
        }
    })
    .to_string();
    format!(
        "HTTP/1.1 {status} {reason}\r\n\
         content-type: application/json\r\n\
         content-length: {}\r\n\
         connection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// Relays bytes from upstream to the client as they arrive.
///
/// Written and flushed per chunk. Nothing is inspected and nothing is retained:
/// this is where a whole conversation passes through, and the only thing learned
/// about it here is how many bytes it was.
pub fn relay<R: Read, W: Write>(from: &mut R, to: &mut W) -> std::io::Result<u64> {
    let mut buffer = vec![0u8; RELAY_CHUNK];
    let mut total = 0u64;
    loop {
        let read = match from.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => n,
            // An upstream that closes without a clean shutdown is ordinary; the
            // client has already had everything that arrived.
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(error) => return Err(error),
        };
        to.write_all(&buffer[..read])?;
        to.flush()?;
        total += read as u64;
    }
    Ok(total)
}

/// How the listener is configured.
///
/// Holds the store rather than a credential. A credential is loaded per request
/// and dropped when the request ends, so a token that was refreshed since the
/// process started is picked up without a restart, and no token sits in this
/// struct where a future `Debug` on it could print one.
pub struct Config {
    pub listen: SocketAddr,
    /// Upstream base, e.g. `https://chatgpt.com/backend-api/codex`.
    pub upstream: String,
    pub store: crate::broker::store::Store,
    /// The accounts this listener may use, in preference order.
    ///
    /// Held separately from the store so that `--account` pins the pool to one
    /// account rather than merely naming the first one tried. An account added to
    /// the store while the broker runs is picked up only if it is named here.
    pub serving: Vec<String>,
    /// What the broker has learned about the pool: usage, refusals, affinity.
    pub pool: Mutex<Pool>,
    /// Which model a request gets. Empty by default: without configuration the
    /// broker does not interfere with what a client asked for.
    pub models: ModelPolicy,
    /// Run authority the broker has issued. Separate from the pool because it
    /// answers a different question: not which account, but whether this
    /// request may spend one at all.
    pub grants: Mutex<Grants>,
    /// How long a client may hold a connection without completing a request.
    pub read_timeout: Duration,
}

/// Most accounts tried for one request before it is refused.
///
/// Bounded so a pool where every account refuses a model costs one round of
/// attempts rather than a retry storm.
const MAX_ATTEMPTS: usize = 4;

/// Largest upstream error body read before deciding whether to rotate.
///
/// Error bodies are small. Successful responses are never read here at all; they
/// are relayed.
const MAX_ERROR_BODY: usize = 64 * 1024;

/// Serves until `stop` reports true, one thread per connection.
pub fn serve(
    config: Arc<Config>,
    counters: Arc<Counters>,
    listener: TcpListener,
    stop: Arc<dyn Fn() -> bool + Send + Sync>,
) -> std::io::Result<()> {
    listener.set_nonblocking(false)?;
    for incoming in listener.incoming() {
        if stop() {
            break;
        }
        let stream = match incoming {
            Ok(stream) => stream,
            Err(_) => continue,
        };
        let config = Arc::clone(&config);
        let counters = Arc::clone(&counters);
        std::thread::spawn(move || {
            counters.connections.fetch_add(1, Ordering::Relaxed);
            let _ = handle(&config, &counters, stream);
        });
    }
    Ok(())
}

/// One client connection.
fn handle(config: &Config, counters: &Counters, stream: TcpStream) -> std::io::Result<()> {
    stream.set_read_timeout(Some(config.read_timeout))?;
    let local = stream.local_addr()?;
    let peer = stream.peer_addr()?;

    // Identity first, before anything is read: a request that cannot be
    // attributed is not forwarded, so the pooled quota is never spent on
    // somebody the report cannot name.
    let uid = match peer_uid(local, peer) {
        Ok(uid) => uid,
        Err(error) => {
            counters
                .requests_refused_unidentified
                .fetch_add(1, Ordering::Relaxed);
            let mut stream = stream;
            let _ = stream.write_all(&refusal(
                403,
                "Forbidden",
                &format!("the broker could not establish who is calling: {error}"),
            ));
            let _ = stream.flush();
            return Ok(());
        }
    };

    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;

    let request = match read_request(&mut reader) {
        Ok(Some(request)) => request,
        Ok(None) => return Ok(()),
        Err(error) => {
            let _ = writer.write_all(&refusal(400, "Bad Request", &format!("{error}")));
            let _ = writer.flush();
            return Ok(());
        }
    };

    // The broker's own endpoints, answered here rather than forwarded. Each is
    // still identified by uid first, like every other request.
    if is_grant_request(&request.target) {
        let response = handle_grant(config, counters, &request, uid);
        writer.write_all(&response)?;
        writer.flush()?;
        return Ok(());
    }

    // Answered here rather than forwarded. A health check must not reach the
    // provider: one that spent quota is one nobody could afford to run often.
    if health::is_health_request(&request.target) {
        let state = health::check(config, counters);
        writer.write_all(&health::response(&state))?;
        writer.flush()?;
        return Ok(());
    }

    // Authority before anything else: a request carrying a grant the broker will
    // not honour is refused before an account is chosen or a body is parsed.
    let presented = header_value(&request.headers, GRANT_HEADER);
    let authority = match &presented {
        Some(token) => match config.grants.lock() {
            Ok(grants) => match grants.check(token, uid, now()) {
                Ok(grant) => Some(Authority {
                    run_id: grant.run_id.clone(),
                    accounts: grant.accounts.clone(),
                    model: grant.model.clone(),
                }),
                Err(why) => {
                    counters.grants_refused.fetch_add(1, Ordering::Relaxed);
                    let _ = writer.write_all(&refusal(403, "Forbidden", &format!("{why}")));
                    let _ = writer.flush();
                    return Ok(());
                }
            },
            Err(_) => None,
        },
        None => None,
    };

    let requested = select::model_of(&request.body);
    let thread = select::thread_of(&request.headers);

    // Policy first: the account is chosen for the model that will actually run,
    // not the one the client asked for. Choosing on the requested model could
    // pick an account that cannot serve the effective one.
    let bound = thread.as_deref().and_then(|thread| {
        config
            .pool
            .lock()
            .ok()
            .and_then(|pool| pool.bound_model(uid, thread, now()).map(str::to_string))
    });
    // A run that pinned a model gets it. Triage chose that model for the task,
    // and the interactive policy is about sessions, not about pipeline work.
    let decision = match authority.as_ref().and_then(|a| a.model.clone()) {
        Some(pinned) => policy::decide(
            &ModelPolicy {
                assign: Some(pinned),
                allowed: Vec::new(),
                version: config.models.version.clone(),
            },
            requested.as_deref(),
            None,
        ),
        None => policy::decide(&config.models, requested.as_deref(), bound.as_deref()),
    };
    let model = decision.effective().map(str::to_string);

    let mut request = request;
    if decision.rewrites()
        && let Some(effective) = decision.effective()
        && let Some(rewritten) = policy::rewrite_model(&request.body, effective)
    {
        request.body = rewritten;
    }
    if let Decision::Substituted {
        requested,
        effective,
    } = &decision
    {
        counters.model_substitutions.fetch_add(1, Ordering::Relaxed);
        eprintln!("timon broker: uid {uid} asked for {requested}; policy assigns {effective}");
    }
    // The uid is not only for the record. It is half of the affinity key, because
    // a thread id arrives in a header and a header is whatever the caller says.
    let caller = thread.as_deref().map(|thread| (uid, thread));

    match attempt(
        config,
        counters,
        &request,
        model.as_deref(),
        caller,
        authority.as_ref(),
    ) {
        Ok(served) => {
            counters
                .bytes_to_upstream
                .fetch_add(request.body.len() as u64, Ordering::Relaxed);
            counters.requests_forwarded.fetch_add(1, Ordering::Relaxed);
            if served.attempts > 1 {
                counters.rotations.fetch_add(1, Ordering::Relaxed);
            }
            // The thread is bound only once a request on it has actually been
            // served, so an account that could not serve the first turn does not
            // capture the conversation.
            if let Some(thread) = thread.as_deref()
                && let Ok(mut pool) = config.pool.lock()
            {
                pool.bind(uid, thread, &served.account, now());
                if let Some(effective) = decision.effective() {
                    pool.bind_model(uid, thread, effective, now());
                }
            }
            let head = announce(&served.head, &policy::headers(&config.models, &decision));
            writer.write_all(&head)?;
            writer.flush()?;
            let mut rest = served.rest;
            let sent = relay(&mut rest, &mut writer)?;
            counters
                .bytes_to_client
                .fetch_add(sent + served.head.len() as u64, Ordering::Relaxed);
        }
        Err(refused) => {
            counters
                .requests_refused_no_account
                .fetch_add(1, Ordering::Relaxed);
            let _ = writer.write_all(&refusal(refused.status, refused.reason, &refused.detail));
            let _ = writer.flush();
        }
    }
    Ok(())
}

/// A response from an account, with its head already read.
struct Served {
    account: String,
    /// The response head, verbatim, to be written to the client unchanged.
    head: Vec<u8>,
    /// Everything after the head, still arriving.
    rest: Box<dyn Read>,
    /// How many accounts were tried, so a rotation can be counted.
    attempts: usize,
}

/// A refusal to send back, in the client's terms.
struct Refused {
    status: u16,
    reason: &'static str,
    detail: String,
}

/// Serves one request, rotating accounts while it is still safe to do so.
///
/// Rotation happens only before anything has been written to the client. Once the
/// first byte of a response has gone out, the request belongs to that account
/// whatever happens next: switching then would splice two answers together.
fn attempt(
    config: &Config,
    counters: &Counters,
    request: &Request,
    model: Option<&str>,
    caller: Option<(u32, &str)>,
    authority: Option<&Authority>,
) -> Result<Served, Refused> {
    let mut candidates = usable_names(config);
    // A run may only spend the accounts it was granted. Applied here rather than
    // in selection so the refusal, when nothing is left, says why.
    if let Some(authority) = authority
        && !authority.accounts.is_empty()
    {
        candidates.retain(|name| authority.accounts.contains(name));
        if candidates.is_empty() {
            // 403 rather than 502, and the difference is not cosmetic: a client
            // reads 502 as "try again" and did, five times, against a condition
            // that cannot change for this run. The run is not permitted these
            // accounts; retrying cannot make it permitted.
            return Err(Refused {
                status: 403,
                reason: "Forbidden",
                detail: format!(
                    "run {} may only spend account(s) [{}], and this broker serves \
                     none of them. Start the run against a broker that does, or grant \
                     it an account this one serves.",
                    authority.run_id,
                    authority.accounts.join(", ")
                ),
            });
        }
    }
    let mut excluded: Vec<String> = Vec::new();
    let mut last: Option<String> = None;
    // Accounts already renewed once for this request, so a provider that keeps
    // refusing cannot turn one request into a refresh loop.
    let mut renewed: Vec<String> = Vec::new();

    for round in 1..=MAX_ATTEMPTS {
        let name = {
            let mut pool = config.pool.lock().map_err(|_| Refused {
                status: 500,
                reason: "Internal Server Error",
                detail: "the broker's account state is poisoned; restart it".to_string(),
            })?;
            match pool.choose(&candidates, model, caller, &excluded, now()) {
                Ok(name) => name,
                Err(why) => {
                    return Err(Refused {
                        status: match why {
                            // 503 for exhaustion: the condition passes when a
                            // window resets, and a client should treat it as
                            // temporary. 502 for a pool that has nothing at all.
                            NoAccount::AllExhausted | NoAccount::AllTried { .. } => 503,
                            NoAccount::NoneServesModel { .. } => 400,
                            NoAccount::PoolEmpty => 502,
                        },
                        reason: "Service Unavailable",
                        detail: match (&why, &last) {
                            (NoAccount::AllExhausted | NoAccount::AllTried { .. }, Some(last)) => {
                                format!("{why}; the last account tried was {last}")
                            }
                            _ => format!("{why}"),
                        },
                    });
                }
            }
        };

        let account = config.store.read(&name);
        let credential = match prepare(&account, counters) {
            Ok(credential) => credential,
            Err(why) => {
                // An account whose credential will not load is out for this
                // request. It is not marked refused for the model, because the
                // model is not what is wrong with it.
                eprintln!("timon broker: account {name} unavailable: {why}");
                excluded.push(name.clone());
                last = Some(name);
                continue;
            }
        };

        // Usage is read only when there is a choice to make. A single-account
        // pool has nothing to decide, and paying for an extra round trip to the
        // provider on every request would be latency spent on no decision.
        if candidates.len() > 1 {
            refresh_standing(config, counters, &name, &credential);
        }

        match open(config, request, &credential) {
            Ok(response) if response.status < 400 => {
                if let Ok(mut pool) = config.pool.lock() {
                    pool.accepted(&name);
                }
                return Ok(Served {
                    account: name,
                    head: response.head,
                    rest: response.rest,
                    attempts: round,
                });
            }
            Ok(response) => {
                let body = read_bounded(response.rest);
                if let Some(model) = model
                    && model_unavailable(response.status, &body)
                {
                    // Proven in testing: an account can list a model in its
                    // catalog and still refuse to serve it. Remember it against
                    // this account and try the next one.
                    if let Ok(mut pool) = config.pool.lock() {
                        pool.refused(&name, model, now());
                    }
                    eprintln!(
                        "timon broker: account {name} cannot serve {model} \
                         (status {}); trying another",
                        response.status
                    );
                    excluded.push(name.clone());
                    last = Some(name);
                    continue;
                }
                if response.status == 401 || response.status == 403 {
                    // A refusal here is *not* the clock. The credential was
                    // renewed before this attempt if it was near expiry, so
                    // either it was invalidated while still valid-looking — a
                    // subscription change does exactly that, and nothing in the
                    // file shows it — or the account is genuinely finished.
                    //
                    // One forced renewal decides which, and it is worth trying
                    // before writing the account off: the alternative is a pool
                    // that silently shrinks whenever somebody changes a plan.
                    if !renewed.contains(&name) {
                        renewed.push(name.clone());
                        match crate::broker::refresh::refresh(&account.home) {
                            Ok(_) => {
                                counters.refreshes.fetch_add(1, Ordering::Relaxed);
                                counters
                                    .refreshes_after_refusal
                                    .fetch_add(1, Ordering::Relaxed);
                                eprintln!(
                                    "timon broker: {name} was refused with an unexpired \
token; renewed it and retrying"
                                );
                                // Not excluded: the same account is tried again,
                                // now with the credential the provider just issued.
                                continue;
                            }
                            Err(error) => eprintln!(
                                "timon broker: {name} was refused and could not be \
renewed ({error}); it needs a fresh login"
                            ),
                        }
                    }
                    if let Ok(mut pool) = config.pool.lock() {
                        pool.rejected(&name, now());
                    }
                    excluded.push(name.clone());
                    last = Some(name);
                    continue;
                }
                // Any other error is the provider's answer to this request and
                // is passed through unchanged, head and body.
                let mut head = response.head;
                head.extend_from_slice(&body);
                return Ok(Served {
                    account: name,
                    head,
                    rest: Box::new(std::io::empty()),
                    attempts: round,
                });
            }
            Err(error) => {
                eprintln!("timon broker: reaching the provider as {name} failed: {error}");
                excluded.push(name.clone());
                last = Some(name);
                continue;
            }
        }
    }

    Err(Refused {
        status: 503,
        reason: "Service Unavailable",
        detail: format!(
            "the broker tried {} account(s) and none could serve the request",
            excluded.len()
        ),
    })
}

/// What a presented grant permits, reduced to what serving a request needs.
struct Authority {
    run_id: String,
    accounts: Vec<String>,
    model: Option<String>,
}

/// The path that mints and revokes run authority.
pub const GRANT_PATH: &str = "/_timon/grant";

fn is_grant_request(target: &str) -> bool {
    let path = target.split('?').next().unwrap_or(target);
    path.trim_end_matches('/') == GRANT_PATH
}

fn header_value(headers: &[(String, String)], want: &str) -> Option<String> {
    headers
        .iter()
        .find(|(name, _)| name == want)
        .map(|(_, value)| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Mints or revokes a grant for the calling principal.
///
/// The uid comes from the kernel, so a grant is always issued to whoever asked
/// and can never be minted on somebody else's behalf.
fn handle_grant(config: &Config, counters: &Counters, request: &Request, uid: u32) -> Vec<u8> {
    if request.method != "POST" {
        return refusal(
            405,
            "Method Not Allowed",
            "POST a run id to mint a grant, or a run id with revoke=true to end one",
        );
    }
    let body: serde_json::Value = match serde_json::from_slice(&request.body) {
        Ok(body) => body,
        Err(_) => return refusal(400, "Bad Request", "the body is not JSON"),
    };
    let Some(run_id) = body.get("run_id").and_then(|v| v.as_str()) else {
        return refusal(400, "Bad Request", "no run_id");
    };

    let Ok(mut grants) = config.grants.lock() else {
        return refusal(
            500,
            "Internal Server Error",
            "the broker's grant state is poisoned; restart it",
        );
    };
    grants.forget_stale(now());

    if body
        .get("revoke")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        let ended = grants.revoke_run(run_id, uid);
        return ok_json(&serde_json::json!({ "run_id": run_id, "revoked": ended }));
    }

    let accounts: Vec<String> = body
        .get("accounts")
        .and_then(|v| v.as_array())
        .map(|list| {
            list.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let model = body
        .get("model")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let lifetime = body
        .get("lifetime_secs")
        .and_then(|v| v.as_i64())
        .unwrap_or(3600);

    match grants.issue(uid, run_id, accounts, model, lifetime, now()) {
        Ok((token, grant)) => {
            counters.grants_issued.fetch_add(1, Ordering::Relaxed);
            ok_json(&serde_json::json!({
                "grant": token,
                "id": grant.id,
                "run_id": grant.run_id,
                "expires_at": grant.expires_at,
                "accounts": grant.accounts,
                "model": grant.model,
                "header": GRANT_HEADER,
            }))
        }
        Err(why) => refusal(500, "Internal Server Error", &why),
    }
}

fn ok_json(value: &serde_json::Value) -> Vec<u8> {
    let body = serde_json::to_string_pretty(value).unwrap_or_else(|_| "{}".to_string());
    format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncache-control: no-store\r\n\
         content-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// Adds headers to a response head, after the status line.
///
/// The client learns which model served it without having to ask. Inserted
/// rather than rebuilt, so everything the provider sent is passed through
/// exactly as it arrived.
fn announce(head: &[u8], extra: &[(String, String)]) -> Vec<u8> {
    if extra.is_empty() {
        return head.to_vec();
    }
    let Some(line_end) = head.windows(2).position(|w| w == b"\r\n") else {
        return head.to_vec();
    };
    let mut out = Vec::with_capacity(head.len() + 64 * extra.len());
    out.extend_from_slice(&head[..line_end + 2]);
    for (name, value) in extra {
        out.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
    }
    out.extend_from_slice(&head[line_end + 2..]);
    out
}

/// The accounts this listener may use that have no fault of their own.
///
/// Re-read per request rather than cached: an account whose credential file was
/// repaired while the broker ran becomes usable again without a restart, and one
/// that broke stops being offered.
fn usable_names(config: &Config) -> Vec<String> {
    config
        .serving
        .iter()
        .filter(|name| config.store.read(name).usable())
        .cloned()
        .collect()
}

/// Loads an account's credential, refreshing it first if it is close to expiry.
///
/// Refresh happens here rather than on a timer because this is the moment the
/// token is about to be used, and a refresh token is single-use: refreshing from
/// two places at once would leave one of them holding a token that is already
/// dead. The per-account lock inside `refresh` is what makes that safe.
fn prepare(account: &Account, counters: &Counters) -> Result<Credential, String> {
    let credential = crate::broker::store::credential_of(account).map_err(|e| format!("{e}"))?;
    if !crate::broker::refresh::due(&credential.bearer, now()) {
        return Ok(credential);
    }
    match crate::broker::refresh::refresh(&account.home) {
        Ok(_) => {
            counters.refreshes.fetch_add(1, Ordering::Relaxed);
            // Re-read: the refresh wrote a new token to the file, and the copy in
            // hand is the old one.
            crate::broker::store::credential_of(account).map_err(|e| format!("{e}"))
        }
        // A refresh that fails is reported and the existing token is still tried.
        // It may have minutes left on it, and a request served is better than a
        // request refused on a prediction.
        Err(error) => {
            eprintln!(
                "timon broker: refreshing {} failed: {error}; using the existing token",
                account.name
            );
            Ok(credential)
        }
    }
}

/// Reads this account's usage from the provider, unless a recent reading stands.
fn refresh_standing(config: &Config, counters: &Counters, name: &str, credential: &Credential) {
    let fresh = config
        .pool
        .lock()
        .ok()
        .and_then(|pool| pool.standing(name).map(|standing| !standing.stale(now())))
        .unwrap_or(false);
    if fresh {
        return;
    }
    let Some(account_id) = credential.account_id.as_deref() else {
        return;
    };
    counters.quota_reads.fetch_add(1, Ordering::Relaxed);
    match crate::broker::quota::read(&credential.bearer, account_id, now()) {
        Ok(standing) => {
            if let Ok(mut pool) = config.pool.lock() {
                pool.observe(name, standing);
            }
        }
        // A usage endpoint that will not answer must not stop the broker serving.
        // Selection treats an account with no reading as eligible.
        Err(error) => eprintln!("timon broker: reading usage for {name} failed: {error}"),
    }
}

/// True when this status and body mean *this account cannot have this model*,
/// as opposed to any other kind of failure.
///
/// Matched on the provider's own wording rather than the status alone, because a
/// 404 also means a mistyped path, and rotating the whole pool over a typo would
/// hide the mistake behind four identical failures.
pub fn model_unavailable(status: u16, body: &[u8]) -> bool {
    if status != 404 && status != 400 {
        return false;
    }
    let text = String::from_utf8_lossy(body).to_lowercase();
    (text.contains("model") && (text.contains("not found") || text.contains("does not exist")))
        || text.contains("model_not_found")
        || text.contains("does not have access to model")
        || text.contains("unsupported_model")
}

/// Reads a bounded amount of an error body.
fn read_bounded(mut from: Box<dyn Read>) -> Vec<u8> {
    let mut body = Vec::new();
    let mut limited = from.by_ref().take(MAX_ERROR_BODY as u64);
    let _ = limited.read_to_end(&mut body);
    body
}

/// Seconds since the epoch.
fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs() as i64)
        .unwrap_or_default()
}

/// An upstream response with its head read and its body still arriving.
struct Response {
    status: u16,
    head: Vec<u8>,
    rest: Box<dyn Read>,
}

/// Opens the upstream connection, sends the rewritten request, and reads back the
/// response head.
///
/// The head is read here and not relayed immediately because the status decides
/// whether this account can serve the request at all. It is kept verbatim so that
/// passing it on later is byte-identical to what the provider sent.
fn open(config: &Config, request: &Request, credential: &Credential) -> std::io::Result<Response> {
    let (scheme, host, port, prefix) = split_upstream(&config.upstream)?;
    let bytes = upstream_request(
        request,
        &prefix,
        &host,
        &credential.bearer,
        credential.account_id.as_deref(),
    );

    let address = format!("{host}:{port}");
    let stream = TcpStream::connect(&address)?;
    stream.set_read_timeout(Some(Duration::from_secs(600)))?;

    let raw: Box<dyn Read> = if scheme == "http" {
        let mut stream = stream;
        stream.write_all(&bytes)?;
        stream.flush()?;
        Box::new(stream)
    } else {
        let mut tls = crate::research::fetch::tls(stream, &host)
            .map_err(|error| std::io::Error::other(format!("{error}")))?;
        tls.write_all(&bytes)?;
        tls.flush()?;
        Box::new(tls)
    };

    let mut reader = BufReader::new(raw);
    let mut head = Vec::new();
    loop {
        let mut line = Vec::new();
        let read = reader.read_until(b'\n', &mut line)?;
        if read == 0 {
            break;
        }
        if head.len() + line.len() > MAX_LINE * MAX_HEADERS {
            return Err(std::io::Error::other("upstream response head is too large"));
        }
        let blank = line == b"\r\n" || line == b"\n";
        head.extend_from_slice(&line);
        if blank {
            break;
        }
    }
    let status =
        status_of(&head).ok_or_else(|| std::io::Error::other("upstream sent no status line"))?;
    Ok(Response {
        status,
        head,
        rest: Box::new(reader),
    })
}

/// Reads the status code out of a response head.
pub fn status_of(head: &[u8]) -> Option<u16> {
    let first = head.split(|byte| *byte == b'\n').next()?;
    let text = String::from_utf8_lossy(first);
    text.split_whitespace().nth(1)?.parse().ok()
}

/// Splits an upstream base into scheme, host, port and path prefix.
pub fn split_upstream(base: &str) -> std::io::Result<(String, String, u16, String)> {
    let (scheme, rest) = base
        .split_once("://")
        .ok_or_else(|| std::io::Error::other("upstream has no scheme"))?;
    let (authority, path) = match rest.find('/') {
        Some(at) => (&rest[..at], &rest[at..]),
        None => (rest, ""),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (
            host.to_string(),
            port.parse()
                .map_err(|_| std::io::Error::other("upstream port is not a number"))?,
        ),
        None => (
            authority.to_string(),
            if scheme == "https" { 443 } else { 80 },
        ),
    };
    if host.is_empty() {
        return Err(std::io::Error::other("upstream has no host"));
    }
    Ok((scheme.to_string(), host, port, path.to_string()))
}
