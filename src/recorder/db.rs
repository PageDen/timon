// Adapted for Timon. Not derived from Prodex source.
//! The append-only SQLite store behind the recorder daemon.
//!
//! The daemon is the only writer. Rows are never updated or deleted: a
//! correction is a new row that points at the one it corrects, so a report can
//! apply it once instead of replacing history.

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::recorder::event::{LATE_AFTER_SECS, StoredEvent, UsageEvent};
use crate::usage::TokenCount;

const SCHEMA_VERSION: i64 = 1;

/// Result of storing one event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Receipt {
    pub id: i64,
    /// The event was already committed under this principal and id, with the
    /// same content. The stored row was returned unchanged.
    pub duplicate: bool,
}

/// Why an append did not store a new row.
#[derive(Debug)]
pub enum AppendError {
    /// The same `(uid, client_event_id)` already holds *different* content.
    /// Never an overwrite: the caller has two different events under one id.
    Conflict {
        existing_id: i64,
    },
    /// The corrected row does not exist, or belongs to another principal.
    UncorrectableTarget {
        corrects: i64,
    },
    Sql(rusqlite::Error),
}

impl std::fmt::Display for AppendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AppendError::Conflict { existing_id } => write!(
                f,
                "event id already stored with different content as row {existing_id}"
            ),
            AppendError::UncorrectableTarget { corrects } => {
                write!(f, "row {corrects} is not yours to correct")
            }
            AppendError::Sql(error) => write!(f, "database error: {error}"),
        }
    }
}

impl std::error::Error for AppendError {}

impl From<rusqlite::Error> for AppendError {
    fn from(error: rusqlite::Error) -> Self {
        AppendError::Sql(error)
    }
}

/// Which rows a reader may see.
#[derive(Clone, Copy, Debug)]
pub enum Scope {
    /// Only this principal's rows. The uid comes from the connection.
    Own(u32),
    /// Every principal's rows, or one named principal's.
    Admin { only_uid: Option<u32> },
}

pub struct Store {
    conn: Connection,
}

impl Store {
    /// Opens or creates the database and applies the schema.
    ///
    /// WAL keeps readers off the writer's back; `synchronous = FULL` is what
    /// makes a committed transaction durable before the daemon acknowledges it,
    /// which is the whole basis of the at-least-once delivery contract.
    pub fn open(path: &Path) -> Result<Self, rusqlite::Error> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let store = Store { conn };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<(), rusqlite::Error> {
        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL);

            CREATE TABLE IF NOT EXISTS usage_events (
                id                      INTEGER PRIMARY KEY AUTOINCREMENT,
                peer_uid                INTEGER NOT NULL,
                peer_username           TEXT,
                client_event_id         TEXT    NOT NULL,
                run_id                  TEXT    NOT NULL,
                attempt_id              TEXT    NOT NULL,
                role                    TEXT    NOT NULL,
                provider                TEXT,
                model                   TEXT,
                profile                 TEXT,
                input_tokens            INTEGER,
                cached_input_tokens     INTEGER,
                output_tokens           INTEGER,
                reasoning_output_tokens INTEGER,
                total_tokens            INTEGER,
                usage_status            TEXT    NOT NULL,
                duration_ms             INTEGER,
                occurred_at             INTEGER NOT NULL,
                received_at             INTEGER NOT NULL,
                late                    INTEGER NOT NULL,
                corrects                INTEGER REFERENCES usage_events(id),
                client_payload          TEXT    NOT NULL,
                UNIQUE (peer_uid, client_event_id)
            );

            CREATE INDEX IF NOT EXISTS usage_events_by_principal
                ON usage_events (peer_uid, occurred_at);

            -- Application-level integrity. These guard the daemon's own code
            -- paths and an operator poking at the file; they are not protection
            -- against root or the file's owner, who can rewrite anything.
            CREATE TRIGGER IF NOT EXISTS usage_events_no_update
            BEFORE UPDATE ON usage_events BEGIN
                SELECT RAISE(ABORT, 'usage_events is append-only');
            END;

            CREATE TRIGGER IF NOT EXISTS usage_events_no_delete
            BEFORE DELETE ON usage_events BEGIN
                SELECT RAISE(ABORT, 'usage_events is append-only');
            END;
            "#,
        )?;
        let recorded: Option<i64> = self
            .conn
            .query_row("SELECT version FROM schema_version", [], |row| row.get(0))
            .optional()?;
        if recorded.is_none() {
            self.conn.execute(
                "INSERT INTO schema_version (version) VALUES (?1)",
                params![SCHEMA_VERSION],
            )?;
        }
        Ok(())
    }

    /// Stores one event for `peer_uid`, idempotently.
    ///
    /// The identity is the caller's, taken from the connection. Retrying a
    /// delivery returns the committed row instead of adding a second one, and
    /// the same id carrying different content is refused rather than allowed to
    /// overwrite what is already stored.
    pub fn append(
        &mut self,
        peer_uid: u32,
        peer_username: Option<&str>,
        event: &UsageEvent,
        received_at: i64,
    ) -> Result<Receipt, AppendError> {
        let payload = event.canonical_payload();
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

        if let Some((id, stored)) = existing(&tx, peer_uid, &event.client_event_id)? {
            // Server-stamped metadata is deliberately outside the comparison, so
            // a replay that arrives later is still recognised as the same event.
            return if stored == payload {
                tx.commit()?;
                Ok(Receipt {
                    id,
                    duplicate: true,
                })
            } else {
                Err(AppendError::Conflict { existing_id: id })
            };
        }

        if let Some(corrects) = event.corrects {
            let owner: Option<i64> = tx
                .query_row(
                    "SELECT peer_uid FROM usage_events WHERE id = ?1",
                    params![corrects],
                    |row| row.get(0),
                )
                .optional()?;
            // Correcting across principals would let one account rewrite
            // another's totals, which is the one thing identity is here to stop.
            if owner != Some(i64::from(peer_uid)) {
                return Err(AppendError::UncorrectableTarget { corrects });
            }
        }

        let late = received_at.saturating_sub(event.occurred_at) > LATE_AFTER_SECS;
        tx.execute(
            r#"
            INSERT INTO usage_events (
                peer_uid, peer_username, client_event_id, run_id, attempt_id, role,
                provider, model, profile,
                input_tokens, cached_input_tokens, output_tokens,
                reasoning_output_tokens, total_tokens,
                usage_status, duration_ms, occurred_at, received_at, late, corrects,
                client_payload
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6,
                ?7, ?8, ?9,
                ?10, ?11, ?12,
                ?13, ?14,
                ?15, ?16, ?17, ?18, ?19, ?20,
                ?21
            )
            "#,
            params![
                i64::from(peer_uid),
                peer_username,
                event.client_event_id,
                event.run_id,
                event.attempt_id,
                event.role.as_str(),
                event.provider,
                event.model,
                event.profile,
                event.usage.input.value().map(sql_count),
                event.usage.cached_input.value().map(sql_count),
                event.usage.output.value().map(sql_count),
                event.usage.reasoning_output.value().map(sql_count),
                event.usage.total().value().map(sql_count),
                status_str(event),
                event.duration_ms.map(sql_count),
                event.occurred_at,
                received_at,
                late as i64,
                event.corrects,
                payload,
            ],
        )?;
        let id = tx.last_insert_rowid();

        // The receipt is only produced after this returns. A caller that never
        // sees the acknowledgement keeps its copy and retries, which the unique
        // key above then collapses back onto this row.
        tx.commit()?;
        Ok(Receipt {
            id,
            duplicate: false,
        })
    }

    /// Reads rows the caller is allowed to see, newest first.
    pub fn query(
        &self,
        scope: Scope,
        since: Option<i64>,
        until: Option<i64>,
        limit: u32,
    ) -> Result<Vec<StoredEvent>, rusqlite::Error> {
        // The uid filter is applied here, in the only place that reads rows, so
        // no caller can reach another principal's data by crafting a request.
        let (uid_clause, uid_param): (&str, Option<i64>) = match scope {
            Scope::Own(uid) => ("AND peer_uid = ?1", Some(i64::from(uid))),
            Scope::Admin {
                only_uid: Some(uid),
            } => ("AND peer_uid = ?1", Some(i64::from(uid))),
            Scope::Admin { only_uid: None } => ("", None),
        };
        let sql = format!(
            r#"
            SELECT id, peer_uid, peer_username, client_event_id, run_id, attempt_id, role,
                   provider, model, profile,
                   input_tokens, cached_input_tokens, output_tokens,
                   reasoning_output_tokens, total_tokens,
                   usage_status, duration_ms, occurred_at, received_at, late, corrects
            FROM usage_events
            WHERE 1 = 1 {uid_clause}
              AND (?2 IS NULL OR occurred_at >= ?2)
              AND (?3 IS NULL OR occurred_at <= ?3)
            ORDER BY id DESC
            LIMIT ?4
            "#
        );
        let mut statement = self.conn.prepare(&sql)?;
        let rows = statement.query_map(params![uid_param, since, until, limit], |row| {
            Ok(StoredEvent {
                id: row.get(0)?,
                peer_uid: row.get::<_, i64>(1)? as u32,
                peer_username: row.get(2)?,
                client_event_id: row.get(3)?,
                run_id: row.get(4)?,
                attempt_id: row.get(5)?,
                role: row.get(6)?,
                provider: row.get(7)?,
                model: row.get(8)?,
                profile: row.get(9)?,
                input: count(row.get::<_, Option<i64>>(10)?),
                cached_input: count(row.get::<_, Option<i64>>(11)?),
                output: count(row.get::<_, Option<i64>>(12)?),
                reasoning_output: count(row.get::<_, Option<i64>>(13)?),
                total: count(row.get::<_, Option<i64>>(14)?),
                usage_status: row.get(15)?,
                duration_ms: row.get::<_, Option<i64>>(16)?.map(|ms| ms as u64),
                occurred_at: row.get(17)?,
                received_at: row.get(18)?,
                late: row.get::<_, i64>(19)? != 0,
                corrects: row.get(20)?,
            })
        })?;
        rows.collect()
    }
}

fn existing(
    tx: &Transaction<'_>,
    peer_uid: u32,
    client_event_id: &str,
) -> Result<Option<(i64, String)>, rusqlite::Error> {
    tx.query_row(
        "SELECT id, client_payload FROM usage_events WHERE peer_uid = ?1 AND client_event_id = ?2",
        params![peer_uid, client_event_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .optional()
}

/// A stored NULL means the producer never reported the number.
fn count(value: Option<i64>) -> TokenCount {
    match value {
        // Non-negative by construction: `UsageEvent::validate` refuses a count
        // that does not fit, so nothing else can get in here.
        Some(value) => TokenCount::Known(value as u64),
        None => TokenCount::Unknown,
    }
}

/// SQLite stores signed 64-bit integers. `UsageEvent::validate` has already
/// refused anything that will not fit, so this cannot silently wrap.
fn sql_count(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn status_str(event: &UsageEvent) -> &'static str {
    match event.usage_status {
        crate::usage::UsageStatus::Complete => "complete",
        crate::usage::UsageStatus::Partial => "partial",
        crate::usage::UsageStatus::Unknown => "unknown",
    }
}

/// Sum of a principal's reported tokens. Unknown parts are excluded and counted
/// separately, because a missing number is not a zero and a total that quietly
/// absorbed it would read as complete.
pub fn totals(events: &[StoredEvent]) -> Totals {
    let mut totals = Totals::default();
    for event in events {
        totals.events += 1;
        match event.total.value() {
            Some(value) => totals.total_tokens = totals.total_tokens.saturating_add(value),
            None => totals.events_with_unknown_usage += 1,
        }
        if event.late {
            totals.late_events += 1;
        }
    }
    totals
}

#[derive(Clone, Copy, Debug, Default, serde::Deserialize, serde::Serialize)]
pub struct Totals {
    pub events: u64,
    pub total_tokens: u64,
    /// Rows whose usage the producer could not report. Their tokens are missing
    /// from `total_tokens`, so a report must show this count beside it.
    pub events_with_unknown_usage: u64,
    pub late_events: u64,
}

/// One principal's totals over a window.
///
/// Sums cover only counts the producer actually reported. Events whose usage is
/// unknown are counted separately rather than folded in as zero, so a total can
/// never read as complete when part of it is missing.
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct PrincipalTotals {
    pub peer_uid: u32,
    /// Latest name seen for this uid. Display only; the number is the identity.
    pub username: Option<String>,
    /// More than one name has been seen for this uid. A rename is the ordinary
    /// cause; a recycled uid is the one that would merge two people's history,
    /// which is why the deployment must not recycle one while records are kept.
    pub names_seen: u64,
    pub events: u64,
    pub total_tokens: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Events the producer could not report usage for. Their tokens are not in
    /// the sums above and are not zero.
    pub events_with_unknown_usage: u64,
    /// Events whose totals are a lower bound.
    pub events_with_partial_usage: u64,
    /// Events the daemon received well after they happened.
    pub late_events: u64,
    /// Corrections counted once each, in place of what they correct.
    pub corrections_applied: u64,
    pub first_occurred_at: Option<i64>,
    pub last_occurred_at: Option<i64>,
}

/// Aggregated usage for a window, with the caveats that must travel with it.
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct Report {
    pub since: Option<i64>,
    pub until: Option<i64>,
    /// Whose usage this covers. `null` means every principal.
    pub scope_uid: Option<u32>,
    pub principals: Vec<PrincipalTotals>,
    /// Rows left out because a later correction replaces them. Each correction
    /// is applied once; it is not an additional charge on top of the original.
    pub superseded_by_corrections: u64,
    /// Always carried with the numbers so a consumer cannot drop them.
    pub basis: String,
    pub quota_note: String,
}

/// What the numbers are, stated wherever they appear.
pub const VISIBILITY_LABEL: &str = "For visibility, not billing. These are figures producers \
reported; an account can under-report, omit, or run a model client directly, so completeness \
cannot be proven.";

/// What the numbers are not.
pub const SHARED_QUOTA_LABEL: &str = "Attributions of shared provider quota, not a separate \
invoice per account.";

impl Store {
    /// Totals a window, grouped by principal.
    ///
    /// Aggregation happens here rather than over fetched rows so a report cannot
    /// be silently truncated by a row limit and understate someone's usage.
    pub fn report(
        &self,
        scope: Scope,
        since: Option<i64>,
        until: Option<i64>,
    ) -> Result<Report, rusqlite::Error> {
        let (uid_clause, uid_param): (&str, Option<i64>) = match scope {
            Scope::Own(uid) => ("AND e.peer_uid = ?1", Some(i64::from(uid))),
            Scope::Admin {
                only_uid: Some(uid),
            } => ("AND e.peer_uid = ?1", Some(i64::from(uid))),
            Scope::Admin { only_uid: None } => ("", None),
        };
        // A row replaced by a correction is excluded, so the correction counts
        // instead of adding to it. Chains resolve naturally: only the row nobody
        // corrects survives.
        let live = format!(
            r#"
            FROM usage_events e
            WHERE NOT EXISTS (SELECT 1 FROM usage_events c WHERE c.corrects = e.id)
              {uid_clause}
              AND (?2 IS NULL OR e.occurred_at >= ?2)
              AND (?3 IS NULL OR e.occurred_at <= ?3)
            "#
        );
        let sql = format!(
            r#"
            SELECT e.peer_uid,
                   (SELECT u.peer_username FROM usage_events u
                     WHERE u.peer_uid = e.peer_uid ORDER BY u.id DESC LIMIT 1),
                   COUNT(DISTINCT e.peer_username),
                   COUNT(*),
                   COALESCE(SUM(e.total_tokens), 0),
                   COALESCE(SUM(e.input_tokens), 0),
                   COALESCE(SUM(e.output_tokens), 0),
                   SUM(e.total_tokens IS NULL),
                   SUM(e.usage_status = 'partial'),
                   SUM(e.late),
                   SUM(e.corrects IS NOT NULL),
                   MIN(e.occurred_at),
                   MAX(e.occurred_at)
            {live}
            GROUP BY e.peer_uid
            ORDER BY e.peer_uid
            "#
        );
        let mut statement = self.conn.prepare(&sql)?;
        let principals = statement
            .query_map(params![uid_param, since, until], |row| {
                Ok(PrincipalTotals {
                    peer_uid: row.get::<_, i64>(0)? as u32,
                    username: row.get(1)?,
                    names_seen: row.get::<_, i64>(2)? as u64,
                    events: row.get::<_, i64>(3)? as u64,
                    total_tokens: row.get::<_, i64>(4)? as u64,
                    input_tokens: row.get::<_, i64>(5)? as u64,
                    output_tokens: row.get::<_, i64>(6)? as u64,
                    events_with_unknown_usage: row.get::<_, i64>(7)? as u64,
                    events_with_partial_usage: row.get::<_, i64>(8)? as u64,
                    late_events: row.get::<_, i64>(9)? as u64,
                    corrections_applied: row.get::<_, i64>(10)? as u64,
                    first_occurred_at: row.get(11)?,
                    last_occurred_at: row.get(12)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        let superseded_sql = format!(
            r#"
            SELECT COUNT(*) FROM usage_events e
            WHERE EXISTS (SELECT 1 FROM usage_events c WHERE c.corrects = e.id)
              {}
              AND (?2 IS NULL OR e.occurred_at >= ?2)
              AND (?3 IS NULL OR e.occurred_at <= ?3)
            "#,
            uid_clause
        );
        let superseded: i64 =
            self.conn
                .query_row(&superseded_sql, params![uid_param, since, until], |row| {
                    row.get(0)
                })?;

        Ok(Report {
            since,
            until,
            scope_uid: match scope {
                Scope::Own(uid) => Some(uid),
                Scope::Admin { only_uid } => only_uid,
            },
            principals,
            superseded_by_corrections: superseded as u64,
            basis: VISIBILITY_LABEL.to_string(),
            quota_note: SHARED_QUOTA_LABEL.to_string(),
        })
    }
}
