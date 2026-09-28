// Adapted for Timon. Not derived from Prodex source.
//! Keeping a pooled account's access token usable.
//!
//! A subscription login stores an access token and a refresh token. The access
//! token expires — the ones observed carry roughly ten days — and once it does,
//! every request through the broker fails with an upstream 401 whose message
//! blames the login rather than the clock. Refreshing is therefore not an
//! optimisation: without it the broker works until it abruptly does not.
//!
//! # What this must not get wrong
//!
//! **Never lose the refresh token.** It is the only thing that can mint a new
//! access token, and the provider does not always return a fresh one. A response
//! that omits it means "keep the one you have", not "you no longer have one".
//! Overwriting it with nothing would require a human to log in again.
//!
//! **Never write a partial credential.** The file is replaced atomically, so a
//! crash mid-write leaves the previous working credential rather than a
//! truncated one.
//!
//! **Never refresh twice at once.** Two concurrent refreshes race to write the
//! file, and a provider may invalidate the older refresh token when it issues a
//! new one — so the loser can be left holding a token that no longer works. A
//! lock file serialises them per account.
//!
//! **Never log a token.** Nothing here returns, formats or records a credential
//! value; errors describe what happened, not what was sent.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// Where a subscription login is refreshed.
pub const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";

/// The Codex CLI's OAuth client, which is what these credentials were issued to.
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

/// Refresh this long before expiry rather than at it.
///
/// A token that expires during a request fails the request. Five minutes is
/// comfortably longer than a turn and short enough not to refresh needlessly.
pub const REFRESH_SKEW_SECS: i64 = 300;

/// Why a refresh did not happen.
#[derive(Debug)]
pub enum RefreshError {
    /// No refresh token stored, so nothing can be minted. A human must log in.
    NotRefreshable,
    /// Another process holds the refresh lock for this account.
    Busy,
    /// The provider refused. Carries its status, never the request body.
    Refused {
        status: u16,
        detail: String,
    },
    Io(String),
    Malformed(&'static str),
}

impl std::fmt::Display for RefreshError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RefreshError::NotRefreshable => write!(
                f,
                "no refresh token is stored for this account, so its access token cannot be \
                 renewed; someone must log in again"
            ),
            RefreshError::Busy => write!(f, "another refresh is already in progress"),
            RefreshError::Refused { status, detail } => {
                write!(f, "the provider refused the refresh ({status}): {detail}")
            }
            RefreshError::Io(what) => write!(f, "{what}"),
            RefreshError::Malformed(what) => write!(f, "{what}"),
        }
    }
}

impl std::error::Error for RefreshError {}

/// When this token stops being accepted, from its own `exp` claim.
///
/// The tokens are JWTs, so expiry is readable without asking the provider. The
/// signature is deliberately not verified: this is used to decide *when to
/// refresh*, not whether to trust anything, and the upstream remains the
/// authority on validity.
pub fn expires_at(token: &str) -> Option<i64> {
    let payload = token.split('.').nth(1)?;
    let decoded = base64url(payload)?;
    let value: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
    value.get("exp").and_then(|exp| exp.as_i64())
}

/// Minimal base64url decoding, so a JWT can be read without a new dependency.
fn base64url(input: &str) -> Option<Vec<u8>> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut buffer = 0u32;
    let mut bits = 0u32;
    for byte in input.bytes() {
        if byte == b'=' {
            break;
        }
        let index = TABLE.iter().position(|candidate| *candidate == byte)? as u32;
        buffer = (buffer << 6) | index;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    Some(out)
}

/// True when this account's access token should be renewed before it is used.
pub fn due(access_token: &str, now: i64) -> bool {
    match expires_at(access_token) {
        Some(exp) => now + REFRESH_SKEW_SECS >= exp,
        // A token whose expiry cannot be read is refreshed rather than trusted:
        // guessing "still valid" fails a request, guessing "renew" costs one call.
        None => true,
    }
}

/// Holds the per-account refresh lock for as long as it is alive.
struct Lock {
    path: PathBuf,
}

impl Lock {
    /// Takes the lock, or reports that someone else has it.
    ///
    /// `create_new` is the whole mechanism: it fails if the file exists, which is
    /// atomic on every filesystem this runs on.
    fn take(home: &Path) -> Result<Self, RefreshError> {
        let path = home.join(".refresh.lock");
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(_) => Ok(Lock { path }),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                // A lock left behind by a killed process would block refreshes
                // forever, so one older than a few minutes is treated as stale.
                let stale = std::fs::metadata(&path)
                    .and_then(|meta| meta.modified())
                    .map(|when| {
                        when.elapsed()
                            .map(|age| age.as_secs() > 300)
                            .unwrap_or(false)
                    })
                    .unwrap_or(false);
                if stale {
                    let _ = std::fs::remove_file(&path);
                    return Lock::take(home);
                }
                Err(RefreshError::Busy)
            }
            Err(error) => Err(RefreshError::Io(format!(
                "taking the refresh lock: {error}"
            ))),
        }
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Renews one account's access token and rewrites its credential file.
///
/// Returns the new expiry. The token itself is not returned, because no caller
/// needs it in hand: they read the store again.
pub fn refresh(home: &Path) -> Result<i64, RefreshError> {
    let _lock = Lock::take(home)?;
    let path = home.join(crate::broker::store::CREDENTIAL_FILE);
    let text = std::fs::read_to_string(&path)
        .map_err(|error| RefreshError::Io(format!("reading the credential: {}", error.kind())))?;
    let mut stored: serde_json::Value = serde_json::from_str(&text)
        .map_err(|_| RefreshError::Malformed("credential is not JSON"))?;

    let refresh_token = stored
        .get("tokens")
        .and_then(|tokens| tokens.get("refresh_token"))
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
        .ok_or(RefreshError::NotRefreshable)?
        .to_string();

    let granted = request(&refresh_token)?;
    let expiry = apply(&mut stored, &granted)?;
    write_atomic(&path, &stored)?;
    Ok(expiry)
}

/// Folds what the provider granted into the stored credential.
///
/// Split out from the request so the rule below can be tested without a network
/// call, because getting it wrong is unrecoverable: a refresh token is
/// **single-use and rotates**, so an account whose refresh token is discarded
/// cannot be renewed again and has to be logged in by hand.
///
/// Returns the new expiry.
pub fn apply(
    stored: &mut serde_json::Value,
    granted: &serde_json::Value,
) -> Result<i64, RefreshError> {
    let tokens = stored
        .get_mut("tokens")
        .and_then(|tokens| tokens.as_object_mut())
        .ok_or(RefreshError::Malformed("credential has no tokens object"))?;

    let access = granted
        .get("access_token")
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
        .ok_or(RefreshError::Malformed(
            "the provider returned no access token",
        ))?;
    tokens.insert(
        "access_token".to_string(),
        serde_json::Value::String(access.to_string()),
    );
    if let Some(id_token) = granted.get("id_token").and_then(|value| value.as_str())
        && !id_token.is_empty()
    {
        tokens.insert(
            "id_token".to_string(),
            serde_json::Value::String(id_token.to_string()),
        );
    }
    // Only replaced when the provider actually issued a new one. An absent or
    // empty refresh token means keep the existing one, never discard it.
    if let Some(rotated) = granted
        .get("refresh_token")
        .and_then(|value| value.as_str())
        && !rotated.is_empty()
    {
        tokens.insert(
            "refresh_token".to_string(),
            serde_json::Value::String(rotated.to_string()),
        );
    }

    let expiry = expires_at(access).unwrap_or(0);
    stored["last_refresh"] = serde_json::Value::String(stamp());
    Ok(expiry)
}

/// Asks the provider for a new access token.
fn request(refresh_token: &str) -> Result<serde_json::Value, RefreshError> {
    let body = serde_json::json!({
        "client_id": CLIENT_ID,
        "grant_type": "refresh_token",
        "refresh_token": refresh_token,
        "scope": "openid profile email",
    })
    .to_string();

    let host = "auth.openai.com";
    let stream = std::net::TcpStream::connect((host, 443))
        .map_err(|error| RefreshError::Io(format!("connecting to {host}: {error}")))?;
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(60)))
        .ok();
    let mut tls = crate::research::fetch::tls(stream, host)
        .map_err(|error| RefreshError::Io(format!("TLS to {host}: {error}")))?;

    let request = format!(
        "POST /oauth/token HTTP/1.1\r\nhost: {host}\r\ncontent-type: application/json\r\n\
         accept: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    tls.write_all(request.as_bytes())
        .map_err(|error| RefreshError::Io(format!("sending the refresh: {error}")))?;
    tls.flush().ok();

    let mut raw = Vec::new();
    // An upstream that closes without a clean TLS shutdown has still delivered
    // its answer; the same tolerance the citation fetcher needed.
    if let Err(error) = tls.read_to_end(&mut raw)
        && raw.is_empty()
    {
        return Err(RefreshError::Io(format!("reading the response: {error}")));
    }

    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or(RefreshError::Malformed("no header break in the response"))?;
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let payload = &raw[split + 4..];

    let status: u16 = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .ok_or(RefreshError::Malformed("no status line"))?;

    let body = decode_body(&head, payload);
    if status != 200 {
        // The provider's own message, truncated. It describes the refusal and
        // does not echo what was sent.
        let detail = String::from_utf8_lossy(&body).chars().take(200).collect();
        return Err(RefreshError::Refused { status, detail });
    }
    serde_json::from_slice(&body).map_err(|_| RefreshError::Malformed("response is not JSON"))
}

/// Undoes chunked transfer encoding when the provider used it.
///
/// Shared with the quota reader, which talks to the same provider over the same
/// hand-rolled HTTP and meets the same framing.
pub fn decode_body(head: &str, payload: &[u8]) -> Vec<u8> {
    if !head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        return payload.to_vec();
    }
    let mut out = Vec::new();
    let mut rest = payload;
    while let Some(line_end) = rest.windows(2).position(|window| window == b"\r\n") {
        let size = usize::from_str_radix(String::from_utf8_lossy(&rest[..line_end]).trim(), 16)
            .unwrap_or(0);
        if size == 0 {
            break;
        }
        let start = line_end + 2;
        let end = (start + size).min(rest.len());
        out.extend_from_slice(&rest[start..end]);
        if end + 2 > rest.len() {
            break;
        }
        rest = &rest[end + 2..];
    }
    out
}

/// Replaces the credential file in one step, never leaving a partial one.
fn write_atomic(path: &Path, value: &serde_json::Value) -> Result<(), RefreshError> {
    let parent = path.parent().unwrap_or(Path::new("."));
    let temp = parent.join(".auth.json.new");
    let serialised = serde_json::to_vec_pretty(value)
        .map_err(|_| RefreshError::Malformed("could not serialise the credential"))?;
    {
        let mut file = std::fs::File::create(&temp)
            .map_err(|error| RefreshError::Io(format!("creating the replacement: {error}")))?;
        restrict(&file)?;
        file.write_all(&serialised)
            .map_err(|error| RefreshError::Io(format!("writing the replacement: {error}")))?;
        file.sync_all()
            .map_err(|error| RefreshError::Io(format!("flushing the replacement: {error}")))?;
    }
    std::fs::rename(&temp, path)
        .map_err(|error| RefreshError::Io(format!("replacing the credential: {error}")))
}

/// Owner-only, before anything is written into it.
#[cfg(unix)]
fn restrict(file: &std::fs::File) -> Result<(), RefreshError> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
        .map_err(|error| RefreshError::Io(format!("restricting the replacement: {error}")))
}

#[cfg(not(unix))]
fn restrict(_file: &std::fs::File) -> Result<(), RefreshError> {
    Ok(())
}

fn stamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    crate::recorder::render::utc(secs as i64).to_string()
}
