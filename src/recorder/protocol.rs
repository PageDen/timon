// Adapted for Timon. Not derived from Prodex source.
//! Line-delimited JSON requests and responses on the recorder socket.

use serde::{Deserialize, Serialize};

use crate::recorder::db::{MonthlyTotals, Report, Totals};
use crate::recorder::event::{StoredEvent, UsageEvent};

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    /// Store one event. Identity is taken from the connection.
    Append { event: Box<UsageEvent> },
    /// Totals a window, grouped by principal. Same authorisation as `Query`.
    Report {
        #[serde(default)]
        since: Option<i64>,
        #[serde(default)]
        until: Option<i64>,
        #[serde(default)]
        only_uid: Option<u32>,
    },
    /// Rolled-up monthly totals, which outlive the detail they came from.
    ///
    /// Reachable by an ordinary account for its own figures, deliberately: once
    /// retention has rolled a month up, this is the only way its owner can still
    /// see what it cost, and a usage figure a person cannot see is not
    /// visibility.
    Monthly {
        #[serde(default)]
        from_month: Option<String>,
        #[serde(default)]
        to_month: Option<String>,
        #[serde(default)]
        only_uid: Option<u32>,
    },
    /// Read rows. `only_uid` is honoured for administrators and refused for
    /// everyone else, rather than silently downgraded to the caller's own rows,
    /// so a client never believes it read another principal's data.
    Query {
        #[serde(default)]
        since: Option<i64>,
        #[serde(default)]
        until: Option<i64>,
        #[serde(default)]
        only_uid: Option<u32>,
        #[serde(default)]
        limit: Option<u32>,
    },
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    /// The event is committed and durable.
    Receipt {
        id: i64,
        /// This delivery matched a row already stored; nothing was added.
        duplicate: bool,
        /// The payload carried a uid/user/admin claim, which was ignored.
        /// Omitted when false, so it must default on the way back in.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        identity_claim_ignored: bool,
    },
    Report {
        report: Box<Report>,
    },
    Monthly {
        months: Vec<MonthlyTotals>,
        /// Whose totals these are. `null` means every principal.
        scope_uid: Option<u32>,
    },
    Rows {
        rows: Vec<StoredEvent>,
        totals: Totals,
        /// Whose rows these are. `null` means every principal.
        scope_uid: Option<u32>,
    },
    Error {
        code: ErrorCode,
        message: String,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// Not valid JSON, or not a known request.
    Malformed,
    /// Larger than the accepted request size.
    TooLarge,
    /// The event cannot be stored as described.
    InvalidEvent,
    /// The same id already holds different content.
    Conflict,
    /// The caller may not do this.
    Forbidden,
    /// Too many requests from this principal.
    RateLimited,
    /// The store failed.
    Storage,
}

pub const MAX_QUERY_LIMIT: u32 = 10_000;
pub const DEFAULT_QUERY_LIMIT: u32 = 1_000;
