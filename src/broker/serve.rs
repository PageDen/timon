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
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::Serialize;

use crate::broker::identity::peer_uid;

/// Where subscription-mode Codex sends its traffic when nothing redirects it.
pub const DEFAULT_UPSTREAM: &str = "https://chat.openai.com/backend-api/codex";

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
const STRIPPED: [&str; 4] = [
    "authorization",
    "chatgpt-account-id",
    "openai-organization",
    "openai-project",
];

/// Running totals, reported when the listener stops.
#[derive(Debug, Default, Serialize)]
pub struct Counters {
    pub connections: AtomicU64,
    pub requests_forwarded: AtomicU64,
    pub requests_refused_unidentified: AtomicU64,
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
pub struct Config {
    pub listen: SocketAddr,
    /// Upstream base, e.g. `https://chat.openai.com/backend-api/codex`.
    pub upstream: String,
    /// Bearer token to inject. Slice 2 serves one account.
    pub bearer: String,
    pub account_id: Option<String>,
    /// How long a client may hold a connection without completing a request.
    pub read_timeout: Duration,
}

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

    counters
        .bytes_to_upstream
        .fetch_add(request.body.len() as u64, Ordering::Relaxed);
    counters.requests_forwarded.fetch_add(1, Ordering::Relaxed);
    // `uid` is what a later slice records alongside the usage; nothing is written
    // here, because slice 2 does not record.
    let _ = uid;

    match forward(config, &request) {
        Ok(mut upstream) => {
            let sent = relay(&mut upstream, &mut writer)?;
            counters.bytes_to_client.fetch_add(sent, Ordering::Relaxed);
        }
        Err(error) => {
            let _ = writer.write_all(&refusal(
                502,
                "Bad Gateway",
                &format!("the broker could not reach the provider: {error}"),
            ));
            let _ = writer.flush();
        }
    }
    Ok(())
}

/// Opens the upstream connection and sends the rewritten request.
///
/// Returns the stream positioned at the response, for the caller to relay.
fn forward(config: &Config, request: &Request) -> std::io::Result<Box<dyn Read>> {
    let (scheme, host, port, prefix) = split_upstream(&config.upstream)?;
    let bytes = upstream_request(
        request,
        &prefix,
        &host,
        &config.bearer,
        config.account_id.as_deref(),
    );

    let address = format!("{host}:{port}");
    let stream = TcpStream::connect(&address)?;
    stream.set_read_timeout(Some(Duration::from_secs(600)))?;

    if scheme == "http" {
        let mut stream = stream;
        stream.write_all(&bytes)?;
        stream.flush()?;
        return Ok(Box::new(stream));
    }

    let mut tls = crate::research::fetch::tls(stream, &host)
        .map_err(|error| std::io::Error::other(format!("{error}")))?;
    tls.write_all(&bytes)?;
    tls.flush()?;
    Ok(Box::new(tls))
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
