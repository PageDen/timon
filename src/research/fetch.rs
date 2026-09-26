// Adapted for Timon. Not derived from Prodex source.
//! A deliberately small HTTP client for checking one cited page.
//!
//! Not a crawler: one URL, no link following beyond redirects, nothing stored.
//! Every bound here exists because the URL came from a model.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

/// Largest body read from a cited page.
pub const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;
/// Redirects followed before giving up.
pub const MAX_REDIRECTS: u8 = 5;
/// Whole-fetch deadline, including every redirect hop.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);

/// Why a page was not fetched.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FetchError {
    /// The URL is not something this is willing to request.
    Rejected(String),
    /// The address it resolves to is not a public destination.
    ///
    /// Kept distinct from `Rejected` so a report can say the difference between
    /// a malformed citation and one aimed at this host's own network.
    BlockedAddress(String),
    TooManyRedirects,
    TooLarge,
    TimedOut,
    Transport(String),
    Status(u16),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::Rejected(why) => write!(f, "the citation was not fetched: {why}"),
            FetchError::BlockedAddress(what) => {
                write!(f, "the citation resolves to a non-public address ({what})")
            }
            FetchError::TooManyRedirects => write!(f, "too many redirects"),
            FetchError::TooLarge => write!(f, "the page exceeds {MAX_BODY_BYTES} bytes"),
            FetchError::TimedOut => write!(f, "the fetch timed out"),
            FetchError::Transport(why) => write!(f, "the fetch failed: {why}"),
            FetchError::Status(code) => write!(f, "the page returned HTTP {code}"),
        }
    }
}

impl std::error::Error for FetchError {}

/// A page, as retrieved.
#[derive(Clone, Debug)]
pub struct Page {
    /// Where the content actually came from, after redirects.
    pub final_url: String,
    pub status: u16,
    pub body: String,
    pub redirects: Vec<String>,
    /// A meta-refresh was followed. Recorded because a verifier that stops at
    /// the first response would see a redirect stub and wrongly conclude the
    /// claim is unsupported.
    pub followed_meta_refresh: bool,
}

/// One HTTP response: status, headers, and a bounded body.
struct Response {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

/// A parsed, accepted URL.
#[derive(Clone, Debug)]
struct Target {
    host: String,
    port: u16,
    path: String,
    tls: bool,
}

/// Parses and screens a URL before anything is connected to.
fn screen(url: &str) -> Result<Target, FetchError> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| FetchError::Rejected(format!("{url:?} has no scheme")))?;
    let tls = match scheme.to_ascii_lowercase().as_str() {
        "https" => true,
        // Only these two. A citation pointing at file://, gopher:// or anything
        // else is not a source, it is an attempt to reach something local.
        "http" => false,
        other => {
            return Err(FetchError::Rejected(format!(
                "scheme {other:?} is not allowed"
            )));
        }
    };
    let (authority, path) = match rest.find(['/', '?', '#']) {
        Some(at) => (&rest[..at], &rest[at..]),
        None => (rest, "/"),
    };
    if authority.contains('@') {
        // Credentials in a citation are either a mistake or an attempt to make
        // the host authenticate somewhere.
        return Err(FetchError::Rejected(
            "the URL carries credentials".to_string(),
        ));
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => (
            host.to_string(),
            port.parse::<u16>()
                .map_err(|_| FetchError::Rejected("the port is not a number".to_string()))?,
        ),
        _ => (authority.to_string(), if tls { 443 } else { 80 }),
    };
    if host.is_empty() {
        return Err(FetchError::Rejected("the URL has no host".to_string()));
    }
    Ok(Target {
        host,
        port,
        path: if path.is_empty() {
            "/".to_string()
        } else {
            path.to_string()
        },
        tls,
    })
}

/// True for an address this host must not be made to reach.
///
/// Checked against the resolved address rather than the name, because a name
/// can resolve anywhere, and re-checked on every redirect hop.
pub fn is_blocked(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_unspecified()
                || v4.is_multicast()
                // 169.254.169.254 and friends are link-local, already covered,
                // but carrier-grade NAT and benchmarking ranges are not.
                || matches!(v4.octets(), [100, b, _, _] if (64..=127).contains(&b))
                || matches!(v4.octets(), [198, 18..=19, _, _])
                || matches!(v4.octets(), [192, 0, 0, _])
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                // Unique local and link-local.
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                || (v6.segments()[0] & 0xffc0) == 0xfe80
                // An IPv4 address wearing a v6 hat still goes to the same place.
                || v6.to_ipv4_mapped().is_some_and(|v4| is_blocked(IpAddr::V4(v4)))
        }
    }
}

/// Resolves a host and returns only addresses that are safe to connect to.
///
/// Resolution happens once per hop and the chosen address is what gets
/// connected to, so a name cannot answer differently for the check and for the
/// connection.
fn resolve(host: &str, port: u16) -> Result<SocketAddr, FetchError> {
    let mut blocked = Vec::new();
    let candidates = (host, port)
        .to_socket_addrs()
        .map_err(|error| FetchError::Transport(format!("{host} did not resolve: {error}")))?;
    for address in candidates {
        if is_blocked(address.ip()) {
            blocked.push(address.ip().to_string());
        } else {
            return Ok(address);
        }
    }
    if blocked.is_empty() {
        Err(FetchError::Transport(format!("{host} resolved to nothing")))
    } else {
        Err(FetchError::BlockedAddress(blocked.join(", ")))
    }
}

/// Fetches one cited page, following redirects within the bounds above.
pub fn get(url: &str, timeout: Duration) -> Result<Page, FetchError> {
    let started = Instant::now();
    let mut current = url.to_string();
    let mut redirects = Vec::new();
    let mut followed_meta_refresh = false;

    for _ in 0..=MAX_REDIRECTS {
        if started.elapsed() >= timeout {
            return Err(FetchError::TimedOut);
        }
        let target = screen(&current)?;
        let address = resolve(&target.host, target.port)?;
        let remaining = timeout.saturating_sub(started.elapsed());
        let response = request(&target, address, remaining)?;
        let Response {
            status,
            headers,
            body,
        } = response;

        if let Some(location) = redirect_target(status, &headers) {
            let next = join(&current, &location)?;
            redirects.push(std::mem::replace(&mut current, next));
            continue;
        }
        if status >= 400 {
            return Err(FetchError::Status(status));
        }

        // A meta-refresh is a redirect the transport never sees. PR3 found a
        // genuine citation behind one: stopping here would report a true claim
        // as unsupported.
        if let Some(location) = meta_refresh(&body) {
            let next = join(&current, &location)?;
            redirects.push(std::mem::replace(&mut current, next));
            followed_meta_refresh = true;
            continue;
        }

        return Ok(Page {
            final_url: current,
            status,
            body,
            redirects,
            followed_meta_refresh,
        });
    }
    Err(FetchError::TooManyRedirects)
}

fn request(
    target: &Target,
    address: SocketAddr,
    timeout: Duration,
) -> Result<Response, FetchError> {
    // The address was screened before we got here, and it is the address we
    // connect to. Resolving again inside a TLS or HTTP library would reopen the
    // gap between what was checked and what is reached.
    let stream = TcpStream::connect_timeout(&address, timeout)
        .map_err(|error| FetchError::Transport(error.to_string()))?;
    stream.set_read_timeout(Some(timeout)).ok();
    stream.set_write_timeout(Some(timeout)).ok();
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: timon-citation-check/0.1\r\n\
Accept: text/html,text/plain\r\nAccept-Encoding: identity\r\nConnection: close\r\n\r\n",
        target.path, target.host
    );

    // One code path for both schemes past this point, so the bounds below cannot
    // be applied to one and forgotten on the other.
    let transport: Box<dyn ReadWrite> = if target.tls {
        Box::new(tls(stream, &target.host)?)
    } else {
        Box::new(stream)
    };
    let mut transport = transport;
    transport
        .write_all(request.as_bytes())
        .map_err(|error| FetchError::Transport(error.to_string()))?;

    let mut reader = BufReader::new(transport);
    let mut status_line = String::new();
    reader
        .read_line(&mut status_line)
        .map_err(|error| FetchError::Transport(error.to_string()))?;
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| FetchError::Transport(format!("bad status line {status_line:?}")))?;

    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        let read = reader
            .read_line(&mut line)
            .map_err(|error| FetchError::Transport(error.to_string()))?;
        if read == 0 || line.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
        }
    }

    // Bounded by one more byte than allowed, so hitting the limit is detectable
    // rather than looking like a page that happened to be exactly that long.
    let mut body = Vec::new();
    reader
        .take(MAX_BODY_BYTES as u64 + 1)
        .read_to_end(&mut body)
        .map_err(|error| FetchError::Transport(error.to_string()))?;
    if body.len() > MAX_BODY_BYTES {
        return Err(FetchError::TooLarge);
    }
    Ok(Response {
        status,
        headers,
        body: String::from_utf8_lossy(&body).to_string(),
    })
}

fn redirect_target(status: u16, headers: &[(String, String)]) -> Option<String> {
    if !matches!(status, 301 | 302 | 303 | 307 | 308) {
        return None;
    }
    headers
        .iter()
        .find(|(name, _)| name == "location")
        .map(|(_, value)| value.clone())
}

/// Anything the request/response code can talk over.
trait ReadWrite: Read + Write {}
impl<T: Read + Write> ReadWrite for T {}

/// Wraps a screened connection in TLS, verifying the certificate against the
/// bundled root store.
///
/// The name is checked against the certificate, so an address that was safe to
/// connect to cannot then present itself as somewhere else.
fn tls(
    stream: TcpStream,
    host: &str,
) -> Result<rustls::StreamOwned<rustls::ClientConnection, TcpStream>, FetchError> {
    use std::sync::Arc;
    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let name = host
        .to_string()
        .try_into()
        .map_err(|_| FetchError::Rejected(format!("{host:?} is not a valid server name")))?;
    let connection = rustls::ClientConnection::new(Arc::new(config), name)
        .map_err(|error| FetchError::Transport(format!("TLS setup failed: {error}")))?;
    Ok(rustls::StreamOwned::new(connection, stream))
}

/// Finds a `<meta http-equiv="refresh" content="0; url=...">` target.
pub fn meta_refresh(body: &str) -> Option<String> {
    let lower = body.to_ascii_lowercase();
    let mut from = 0;
    while let Some(at) = lower[from..].find("<meta") {
        let start = from + at;
        let end = lower[start..].find('>').map(|e| start + e)?;
        let tag = &lower[start..end];
        if tag.contains("http-equiv")
            && tag.contains("refresh")
            && let Some(url_at) = tag.find("url=")
        {
            let value = &body[start + url_at + 4..end];
            let value = value.trim().trim_matches(['"', '\'', ' ']);
            let value = value.split(['"', '\'', ' ']).next().unwrap_or("");
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
        from = end;
    }
    None
}

/// Resolves a redirect target against the URL it came from.
fn join(base: &str, location: &str) -> Result<String, FetchError> {
    if location.contains("://") {
        return Ok(location.to_string());
    }
    let (scheme, rest) = base
        .split_once("://")
        .ok_or_else(|| FetchError::Rejected("the base URL has no scheme".to_string()))?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    if let Some(absolute) = location.strip_prefix('/') {
        Ok(format!("{scheme}://{authority}/{absolute}"))
    } else {
        let path = rest.strip_prefix(authority).unwrap_or("/");
        let dir = path.rsplit_once('/').map(|(head, _)| head).unwrap_or("");
        Ok(format!("{scheme}://{authority}{dir}/{location}"))
    }
}
