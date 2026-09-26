// Adapted for Timon. Not derived from Prodex source.
//! The usage event a producer reports, and the row the daemon stores.
//!
//! A producer describes only what it observed. Identity and receipt time are
//! stamped by the daemon from the connection, never taken from the payload, so
//! nothing a client writes can attribute usage to another account.

use serde::{Deserialize, Serialize};

use crate::attempt::Role;
use crate::usage::{TokenCount, TokenUsage, UsageStatus};

/// Wire format version this build speaks.
pub const PROTOCOL_VERSION: u32 = 1;

/// How far behind `occurred_at` a receipt may be before the row is marked late.
pub const LATE_AFTER_SECS: i64 = 300;

/// Largest accepted request, in bytes.
pub const MAX_REQUEST_BYTES: usize = 64 * 1024;

/// What a producer reports about one attempt.
///
/// `uid`, `user` and `admin` are accepted and **discarded**: older or hostile
/// clients may send them, and rejecting the message outright would turn a
/// harmless claim into lost usage. They are excluded from the stored payload, so
/// they cannot affect deduplication either. Any other unknown field is refused,
/// because that is far more likely to be a misspelled real field.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UsageEvent {
    pub version: u32,
    /// Stable per-attempt id. A retry or a replay after a restart repeats it.
    pub client_event_id: String,
    pub run_id: String,
    pub attempt_id: String,
    pub role: Role,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Non-secret profile label, when the producer uses one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(default)]
    pub usage: TokenUsage,
    pub usage_status: UsageStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// Producer's clock, in Unix seconds. Preserved as reported.
    pub occurred_at: i64,
    /// Id of an earlier event of the same principal that this one corrects.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub corrects: Option<i64>,

    // Ignored identity claims. Present so they parse; never read.
    #[serde(default, skip_serializing)]
    #[allow(dead_code)]
    uid: Option<serde_json::Value>,
    #[serde(default, skip_serializing)]
    #[allow(dead_code)]
    user: Option<serde_json::Value>,
    #[serde(default, skip_serializing)]
    #[allow(dead_code)]
    admin: Option<serde_json::Value>,
}

impl UsageEvent {
    /// Builds an event from values a producer observed.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        client_event_id: String,
        run_id: String,
        attempt_id: String,
        role: Role,
        provider: Option<String>,
        model: Option<String>,
        usage: TokenUsage,
        usage_status: UsageStatus,
        duration_ms: Option<u64>,
        occurred_at: i64,
    ) -> Self {
        UsageEvent {
            version: PROTOCOL_VERSION,
            client_event_id,
            run_id,
            attempt_id,
            role,
            provider,
            model,
            profile: None,
            usage,
            usage_status,
            duration_ms,
            occurred_at,
            corrects: None,
            uid: None,
            user: None,
            admin: None,
        }
    }

    /// True when the payload carried an identity claim the daemon ignored.
    pub fn carried_identity_claim(&self) -> bool {
        self.uid.is_some() || self.user.is_some() || self.admin.is_some()
    }

    /// The client-meaningful content, canonically encoded.
    ///
    /// Two deliveries of one event must produce the same string, so this omits
    /// everything the server decides (identity, receipt time, lateness) and
    /// every ignored identity claim. Field order is the declaration order of
    /// [`CanonicalPayload`], which makes the encoding stable across runs.
    pub fn canonical_payload(&self) -> String {
        let canonical = CanonicalPayload {
            version: self.version,
            client_event_id: &self.client_event_id,
            run_id: &self.run_id,
            attempt_id: &self.attempt_id,
            role: self.role,
            provider: self.provider.as_deref(),
            model: self.model.as_deref(),
            profile: self.profile.as_deref(),
            usage: self.usage,
            usage_status: self.usage_status,
            duration_ms: self.duration_ms,
            occurred_at: self.occurred_at,
            corrects: self.corrects,
        };
        serde_json::to_string(&canonical).expect("canonical payload is serialisable")
    }

    /// Rejects an event this build cannot store faithfully.
    pub fn validate(&self) -> Result<(), EventError> {
        if self.version != PROTOCOL_VERSION {
            return Err(EventError::UnsupportedVersion(self.version));
        }
        if self.client_event_id.is_empty() {
            return Err(EventError::Empty("client_event_id"));
        }
        if self.run_id.is_empty() {
            return Err(EventError::Empty("run_id"));
        }
        if self.attempt_id.is_empty() {
            return Err(EventError::Empty("attempt_id"));
        }
        // A subset larger than the whole is a producer miscount, not a number to
        // store and report as if it were meaningful.
        if let (Some(input), Some(cached)) =
            (self.usage.input.value(), self.usage.cached_input.value())
            && cached > input
        {
            return Err(EventError::SubsetTooLarge("cached_input", "input"));
        }
        if let (Some(output), Some(reasoning)) = (
            self.usage.output.value(),
            self.usage.reasoning_output.value(),
        ) && reasoning > output
        {
            return Err(EventError::SubsetTooLarge("reasoning_output", "output"));
        }
        // SQLite holds signed 64-bit integers. Refusing here is what lets the
        // store cast without wrapping a count into a negative number.
        for (label, count) in [
            ("input", self.usage.input),
            ("cached_input", self.usage.cached_input),
            ("output", self.usage.output),
            ("reasoning_output", self.usage.reasoning_output),
        ] {
            if let Some(value) = count.value()
                && i64::try_from(value).is_err()
            {
                return Err(EventError::Unstorable(label));
            }
        }
        Ok(())
    }
}

/// Exactly the fields that identify one event's content.
#[derive(Serialize)]
struct CanonicalPayload<'a> {
    version: u32,
    client_event_id: &'a str,
    run_id: &'a str,
    attempt_id: &'a str,
    role: Role,
    provider: Option<&'a str>,
    model: Option<&'a str>,
    profile: Option<&'a str>,
    usage: TokenUsage,
    usage_status: UsageStatus,
    duration_ms: Option<u64>,
    occurred_at: i64,
    corrects: Option<i64>,
}

/// Why an event cannot be stored.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EventError {
    UnsupportedVersion(u32),
    Empty(&'static str),
    SubsetTooLarge(&'static str, &'static str),
    /// Larger than the store can hold without changing the number.
    Unstorable(&'static str),
}

impl std::fmt::Display for EventError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EventError::UnsupportedVersion(version) => write!(
                f,
                "unsupported event version {version}; this build speaks {PROTOCOL_VERSION}"
            ),
            EventError::Empty(field) => write!(f, "{field} must not be empty"),
            EventError::SubsetTooLarge(part, whole) => {
                write!(f, "{part} exceeds {whole}, which cannot be true")
            }
            EventError::Unstorable(field) => {
                write!(f, "{field} is too large to store without altering it")
            }
        }
    }
}

impl std::error::Error for EventError {}

/// One stored row, as reported back to an authorised reader.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct StoredEvent {
    pub id: i64,
    pub peer_uid: u32,
    /// Resolved by the server for display. The uid is the authority.
    pub peer_username: Option<String>,
    pub client_event_id: String,
    pub run_id: String,
    pub attempt_id: String,
    pub role: String,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub profile: Option<String>,
    pub input: TokenCount,
    pub cached_input: TokenCount,
    pub output: TokenCount,
    pub reasoning_output: TokenCount,
    pub total: TokenCount,
    pub usage_status: String,
    pub duration_ms: Option<u64>,
    pub occurred_at: i64,
    pub received_at: i64,
    /// The daemon saw this event well after the producer observed it.
    pub late: bool,
    pub corrects: Option<i64>,
}
