// Adapted for Timon. Not derived from Prodex source.
//! What an account has left, asked of the provider rather than inferred.
//!
//! Selection needs to know which account can serve the next request. Guessing
//! from local counters drifts: usage from outside this broker is invisible to it,
//! and a window reset moves. The provider answers both questions directly, so
//! this asks.
//!
//! Read before a request is committed, never during one. A rotation decision
//! taken mid-stream would abandon a partly-delivered answer.

use std::io::{Read, Write};

/// Where the provider reports usage against a subscription.
pub const USAGE_URL_HOST: &str = "chatgpt.com";
pub const USAGE_URL_PATH: &str = "/backend-api/wham/usage";

/// How long a reading is trusted before asking again.
///
/// Short enough that an account exhausted by someone else is noticed quickly,
/// long enough that a burst of requests does not become a burst of quota calls.
pub const CACHE_SECS: i64 = 60;

/// One account's standing with the provider.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Standing {
    pub account_id: String,
    pub email: Option<String>,
    pub plan_type: Option<String>,
    /// The provider's own verdict on whether this account may be used now.
    pub allowed: bool,
    pub limit_reached: bool,
    /// Percentage of the primary window consumed, when reported.
    pub used_percent: Option<u32>,
    /// When the primary window resets, as unix seconds.
    pub reset_at: Option<i64>,
    /// When this reading was taken.
    pub read_at: i64,
}

impl Standing {
    /// True when the provider says this account can serve a request.
    ///
    /// `allowed` is the provider's answer and is trusted over the percentage: a
    /// window can report low usage and still be refused for reasons not visible
    /// here, such as a spend control.
    pub fn usable(&self) -> bool {
        self.allowed && !self.limit_reached
    }

    /// How much room is left, for ranking. Unknown usage ranks as half full so an
    /// unmeasured account is neither preferred nor excluded.
    pub fn headroom(&self) -> u32 {
        match self.used_percent {
            Some(used) => 100u32.saturating_sub(used),
            None => 50,
        }
    }

    pub fn stale(&self, now: i64) -> bool {
        now - self.read_at > CACHE_SECS
    }
}

/// Why a reading could not be taken.
#[derive(Debug)]
pub enum QuotaError {
    /// The credential was refused. Usually means it needs refreshing.
    Unauthorized,
    Refused {
        status: u16,
    },
    Io(String),
    Malformed(&'static str),
}

impl std::fmt::Display for QuotaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            QuotaError::Unauthorized => write!(
                f,
                "the provider refused the credential when asked for usage; it may need refreshing"
            ),
            QuotaError::Refused { status } => write!(f, "the usage endpoint returned {status}"),
            QuotaError::Io(what) => write!(f, "{what}"),
            QuotaError::Malformed(what) => write!(f, "{what}"),
        }
    }
}

impl std::error::Error for QuotaError {}

/// Asks the provider what this account has left.
///
/// Takes the bearer and account id rather than reading the store, so the caller
/// controls when a credential is loaded and this function never touches a file.
pub fn read(bearer: &str, account_id: &str, now: i64) -> Result<Standing, QuotaError> {
    let stream = std::net::TcpStream::connect((USAGE_URL_HOST, 443))
        .map_err(|error| QuotaError::Io(format!("connecting: {error}")))?;
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(30)))
        .ok();
    let mut tls = crate::research::fetch::tls(stream, USAGE_URL_HOST)
        .map_err(|error| QuotaError::Io(format!("TLS: {error}")))?;

    let request = format!(
        "GET {USAGE_URL_PATH} HTTP/1.1\r\nhost: {USAGE_URL_HOST}\r\n\
         authorization: Bearer {bearer}\r\nchatgpt-account-id: {account_id}\r\n\
         accept: application/json\r\nuser-agent: timon-broker\r\nconnection: close\r\n\r\n"
    );
    tls.write_all(request.as_bytes())
        .map_err(|error| QuotaError::Io(format!("sending: {error}")))?;
    tls.flush().ok();

    let mut raw = Vec::new();
    if let Err(error) = tls.read_to_end(&mut raw)
        && raw.is_empty()
    {
        return Err(QuotaError::Io(format!("reading: {error}")));
    }

    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or(QuotaError::Malformed("no header break"))?;
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let status: u16 = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .ok_or(QuotaError::Malformed("no status line"))?;
    if status == 401 || status == 403 {
        return Err(QuotaError::Unauthorized);
    }
    if status != 200 {
        return Err(QuotaError::Refused { status });
    }

    let body = crate::broker::refresh::decode_body(&head, &raw[split + 4..]);
    let value: serde_json::Value =
        serde_json::from_slice(&body).map_err(|_| QuotaError::Malformed("response is not JSON"))?;
    Ok(parse(&value, account_id, now))
}

/// Reads a standing out of the provider's reply.
///
/// Separated from the request so it can be tested against captured shapes. Every
/// field is optional on purpose: this endpoint is undocumented, and a missing
/// field must degrade the reading rather than fail it.
pub fn parse(value: &serde_json::Value, account_id: &str, now: i64) -> Standing {
    let limits = value.get("rate_limit");
    let primary = limits.and_then(|l| l.get("primary_window"));
    Standing {
        account_id: value
            .get("account_id")
            .and_then(|v| v.as_str())
            .unwrap_or(account_id)
            .to_string(),
        email: value
            .get("email")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        plan_type: value
            .get("plan_type")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        // Absent `allowed` is treated as allowed: refusing every account because
        // one field moved would take the whole host offline over a schema change.
        allowed: limits
            .and_then(|l| l.get("allowed"))
            .and_then(|v| v.as_bool())
            .unwrap_or(true),
        limit_reached: limits
            .and_then(|l| l.get("limit_reached"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        used_percent: primary
            .and_then(|w| w.get("used_percent"))
            .and_then(|v| v.as_u64())
            .map(|used| used as u32),
        reset_at: primary
            .and_then(|w| w.get("reset_at"))
            .and_then(|v| v.as_i64()),
        read_at: now,
    }
}
