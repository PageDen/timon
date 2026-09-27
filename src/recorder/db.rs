// Adapted for Timon. Not derived from Prodex source.
//! The append-only SQLite store behind the recorder daemon.
//!
//! The daemon is the only writer. Rows are never updated or deleted: a
//! correction is a new row that points at the one it corrects, so a report can
//! apply it once instead of replacing history.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::recorder::event::{LATE_AFTER_SECS, StoredEvent, UsageEvent};
use crate::usage::TokenCount;

const SCHEMA_VERSION: i64 = 2;

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
    /// Only this principal's rows, and only its current generation. The uid
    /// comes from the connection.
    ///
    /// Restricting to the current generation is what makes a recycled uid safe
    /// to report on: a new holder of uid 1001 sees its own usage and not the
    /// history of whoever held 1001 before the account was retired.
    Own(u32),
    /// Every principal's rows, across every generation, or one named
    /// principal's. An administrator can see a retired generation; the account
    /// that inherited its number cannot.
    Admin { only_uid: Option<u32> },
}

/// Current generation of a uid, as a scalar subquery.
///
/// The generation *is* the number of recorded retirements: a uid nobody has
/// retired is generation 0, and each boundary moves it on by one. Deriving it
/// rather than storing it in a second place means the two can never disagree.
const CURRENT_GENERATION: &str =
    "(SELECT COUNT(*) FROM principal_retirements r WHERE r.peer_uid = ?1)";

/// The `WHERE` fragment for a scope, and the uid it binds to `?1`.
///
/// `prefix` is the table alias used by the caller's query, with its dot.
fn scope_clause(scope: Scope, prefix: &str) -> (String, Option<i64>) {
    match scope {
        Scope::Own(uid) => (
            format!(
                "AND {prefix}peer_uid = ?1 AND {prefix}principal_generation = {CURRENT_GENERATION}"
            ),
            Some(i64::from(uid)),
        ),
        Scope::Admin {
            only_uid: Some(uid),
        } => (format!("AND {prefix}peer_uid = ?1"), Some(i64::from(uid))),
        Scope::Admin { only_uid: None } => (String::new(), None),
    }
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
        // Version first, and the v1 rebuild before anything else: the batch below
        // creates an index over `principal_generation`, which a v1 table does not
        // have yet. Creating the schema before migrating it fails on exactly the
        // databases the migration exists for.
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL);",
        )?;
        let recorded: Option<i64> = self
            .conn
            .query_row("SELECT version FROM schema_version", [], |row| row.get(0))
            .optional()?;
        if recorded == Some(1) {
            self.migrate_v1_to_v2()?;
        }

        self.conn.execute_batch(
            r#"
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
                -- Which generation of `peer_uid` this belongs to: the number of
                -- retirements recorded for that uid when the event was stored.
                -- Part of the dedup key because `client_event_id` is derived
                -- from role, run id and attempt id rather than generated, so two
                -- people who happen to share a recycled uid can easily produce
                -- the same id and must not collide.
                principal_generation    INTEGER NOT NULL DEFAULT 0,
                UNIQUE (peer_uid, principal_generation, client_event_id)
            );

            CREATE INDEX IF NOT EXISTS usage_events_by_principal
                ON usage_events (peer_uid, principal_generation, occurred_at);

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

            -- A recorded boundary in one uid's history. The kernel reuses uids,
            -- so a uid alone does not identify a person over time: when an
            -- account is removed, a row here says so, and every event recorded
            -- afterwards belongs to a later generation of that number. Rows on
            -- either side of a boundary are never totalled together for the
            -- account itself, which is what stops a new holder of uid 1001 from
            -- reading the previous holder's usage.
            CREATE TABLE IF NOT EXISTS principal_retirements (
                peer_uid          INTEGER NOT NULL,
                retired_at        INTEGER NOT NULL,
                retired_username  TEXT,
                note              TEXT,
                recorded_at       INTEGER NOT NULL,
                PRIMARY KEY (peer_uid, retired_at)
            );

            CREATE TRIGGER IF NOT EXISTS principal_retirements_no_update
            BEFORE UPDATE ON principal_retirements BEGIN
                SELECT RAISE(ABORT, 'principal_retirements is append-only');
            END;

            CREATE TRIGGER IF NOT EXISTS principal_retirements_no_delete
            BEFORE DELETE ON principal_retirements BEGIN
                SELECT RAISE(ABORT, 'principal_retirements is append-only');
            END;

            -- Detail rows aged out by retention, rolled up per account, per
            -- generation, per UTC month. Aggregates are kept after the detail
            -- they came from is gone, because a monthly total ages far better
            -- than an event row: it answers "what did this cost" without
            -- holding a record of each individual thing someone ran.
            --
            -- Only live rows are rolled up. A row replaced by a correction is
            -- not counted here any more than it is counted by a report, so the
            -- two agree across the retention boundary.
            CREATE TABLE IF NOT EXISTS usage_monthly (
                peer_uid                  INTEGER NOT NULL,
                principal_generation      INTEGER NOT NULL,
                month                     TEXT    NOT NULL,
                peer_username             TEXT,
                events                    INTEGER NOT NULL,
                total_tokens              INTEGER NOT NULL,
                input_tokens              INTEGER NOT NULL,
                output_tokens             INTEGER NOT NULL,
                events_with_unknown_usage INTEGER NOT NULL,
                events_with_partial_usage INTEGER NOT NULL,
                first_occurred_at         INTEGER,
                last_occurred_at          INTEGER,
                rolled_up_at              INTEGER NOT NULL,
                PRIMARY KEY (peer_uid, principal_generation, month)
            );

            -- What retention has actually done to this database. A report whose
            -- window reaches back past `detail_from` would otherwise read as an
            -- absence of usage rather than an absence of detail, which is the
            -- same silent understatement the unknown-usage handling exists to
            -- avoid.
            CREATE TABLE IF NOT EXISTS retention_state (
                singleton     INTEGER PRIMARY KEY CHECK (singleton = 0),
                detail_from   INTEGER,
                keep_days     INTEGER,
                last_run_at   INTEGER,
                rows_rolled   INTEGER NOT NULL DEFAULT 0
            );
            "#,
        )?;
        if recorded.is_none() {
            self.conn.execute(
                "INSERT INTO schema_version (version) VALUES (?1)",
                params![SCHEMA_VERSION],
            )?;
        }
        Ok(())
    }

    /// Rebuilds `usage_events` so the dedup key includes the generation.
    ///
    /// `CREATE TABLE IF NOT EXISTS` above leaves an existing v1 table alone, and
    /// SQLite cannot alter a UNIQUE constraint in place, so the table is copied
    /// into the new shape and swapped. This is the one code path that removes
    /// rows from an append-only table, which is why it counts them first and
    /// aborts the whole transaction if the copy is not exact: a migration that
    /// silently dropped usage would destroy the record it exists to preserve.
    ///
    /// `DROP TABLE` does not fire row triggers, so the append-only guards do not
    /// need lifting to do this.
    fn migrate_v1_to_v2(&self) -> Result<(), rusqlite::Error> {
        let tx_sql = r#"
            CREATE TABLE usage_events_v2 (
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
                corrects                INTEGER REFERENCES usage_events_v2(id),
                client_payload          TEXT    NOT NULL,
                principal_generation    INTEGER NOT NULL DEFAULT 0,
                UNIQUE (peer_uid, principal_generation, client_event_id)
            );

            -- Ids are carried over, not reassigned, so `corrects` keeps pointing
            -- at the row it corrects. Every pre-existing row is generation 0:
            -- nothing was retired before this column existed, so there is no
            -- boundary to place them after.
            INSERT INTO usage_events_v2 (
                id, peer_uid, peer_username, client_event_id, run_id, attempt_id,
                role, provider, model, profile,
                input_tokens, cached_input_tokens, output_tokens,
                reasoning_output_tokens, total_tokens,
                usage_status, duration_ms, occurred_at, received_at, late,
                corrects, client_payload, principal_generation
            )
            SELECT
                id, peer_uid, peer_username, client_event_id, run_id, attempt_id,
                role, provider, model, profile,
                input_tokens, cached_input_tokens, output_tokens,
                reasoning_output_tokens, total_tokens,
                usage_status, duration_ms, occurred_at, received_at, late,
                corrects, client_payload, 0
            FROM usage_events;
        "#;

        let tx = self.conn.unchecked_transaction().and_then(|tx| {
            tx.execute_batch("PRAGMA defer_foreign_keys = ON;")?;
            Ok(tx)
        })?;

        let before: (i64, i64) = tx.query_row(
            "SELECT COUNT(*), COALESCE(SUM(total_tokens), 0) FROM usage_events",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        tx.execute_batch(tx_sql)?;
        let after: (i64, i64) = tx.query_row(
            "SELECT COUNT(*), COALESCE(SUM(total_tokens), 0) FROM usage_events_v2",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if before != after {
            // Rolled back by the drop below never running: returning here drops
            // the transaction, which rolls it back.
            return Err(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CORRUPT),
                Some(format!(
                    "migration would have changed the record: {} rows and {} tokens before, \
                 {} rows and {} tokens after",
                    before.0, before.1, after.0, after.1
                )),
            ));
        }

        tx.execute_batch(
            r#"
            DROP TABLE usage_events;
            ALTER TABLE usage_events_v2 RENAME TO usage_events;

            CREATE INDEX IF NOT EXISTS usage_events_by_principal
                ON usage_events (peer_uid, principal_generation, occurred_at);

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
        tx.execute("UPDATE schema_version SET version = ?1", params![2])?;
        tx.commit()?;
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

        // Resolved inside the transaction that stores the row, so a retirement
        // recorded concurrently cannot land the event in a generation that was
        // current when the request arrived but is not when it commits.
        let generation = generation_now(&tx, peer_uid)?;

        if let Some((id, stored)) = existing(&tx, peer_uid, generation, &event.client_event_id)? {
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
            let owner: Option<(i64, i64)> = tx
                .query_row(
                    "SELECT peer_uid, principal_generation FROM usage_events WHERE id = ?1",
                    params![corrects],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            // Correcting across principals would let one account rewrite
            // another's totals, which is the one thing identity is here to stop.
            // The generation is part of that: inheriting a recycled uid must not
            // carry the right to rewrite the previous holder's figures.
            if owner != Some((i64::from(peer_uid), i64::from(generation))) {
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
                client_payload, principal_generation
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6,
                ?7, ?8, ?9,
                ?10, ?11, ?12,
                ?13, ?14,
                ?15, ?16, ?17, ?18, ?19, ?20,
                ?21, ?22
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
                i64::from(generation),
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
        let (uid_clause, uid_param) = scope_clause(scope, "");
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
    generation: u32,
    client_event_id: &str,
) -> Result<Option<(i64, String)>, rusqlite::Error> {
    tx.query_row(
        "SELECT id, client_payload FROM usage_events \
         WHERE peer_uid = ?1 AND principal_generation = ?2 AND client_event_id = ?3",
        params![peer_uid, generation, client_event_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .optional()
}

/// How many times this uid has been retired, which is its current generation.
fn generation_now(tx: &Transaction<'_>, peer_uid: u32) -> Result<u32, rusqlite::Error> {
    let count: i64 = tx.query_row(
        "SELECT COUNT(*) FROM principal_retirements WHERE peer_uid = ?1",
        params![peer_uid],
        |row| row.get(0),
    )?;
    Ok(count as u32)
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
    /// Which generation of `peer_uid` these totals belong to. Generations are
    /// never summed together: a uid recycled after an account was removed is two
    /// different people, and adding their figures up would be the exact error
    /// the boundary exists to prevent.
    pub principal_generation: u32,
    /// Latest name seen for this uid. Display only; the number is the identity.
    pub username: Option<String>,
    /// More than one name has been seen for this uid *within this generation*. A
    /// rename is the ordinary cause. A recycled uid used to be the alarming one,
    /// because it merged two people's history; recording a retirement boundary
    /// now separates them, so this is a rename signal rather than a warning. It
    /// stays reported because an operator who recycles a uid *without* recording
    /// the retirement still has the old problem, and this is how it shows.
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
    /// The instant from which detail is complete, when retention has pruned
    /// anything. `None` means no detail has ever been pruned, so the window is
    /// covered in full.
    ///
    /// This is retention's cutoff rather than the oldest surviving row: some
    /// older rows can survive as part of a correction chain, and treating those
    /// as coverage would overstate what the database can answer.
    pub detail_from: Option<i64>,
    /// Set when the requested window reaches back before `detail_from`, so a
    /// pruned stretch cannot be read as a stretch where nobody ran anything.
    pub detail_incomplete: bool,
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
        let (uid_clause, uid_param) = scope_clause(scope, "e.");
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
                   e.principal_generation,
                   (SELECT u.peer_username FROM usage_events u
                     WHERE u.peer_uid = e.peer_uid
                       AND u.principal_generation = e.principal_generation
                     ORDER BY u.id DESC LIMIT 1),
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
            GROUP BY e.peer_uid, e.principal_generation
            ORDER BY e.peer_uid, e.principal_generation
            "#
        );
        let mut statement = self.conn.prepare(&sql)?;
        let principals = statement
            .query_map(params![uid_param, since, until], |row| {
                Ok(PrincipalTotals {
                    peer_uid: row.get::<_, i64>(0)? as u32,
                    principal_generation: row.get::<_, i64>(1)? as u32,
                    username: row.get(2)?,
                    names_seen: row.get::<_, i64>(3)? as u64,
                    events: row.get::<_, i64>(4)? as u64,
                    total_tokens: row.get::<_, i64>(5)? as u64,
                    input_tokens: row.get::<_, i64>(6)? as u64,
                    output_tokens: row.get::<_, i64>(7)? as u64,
                    events_with_unknown_usage: row.get::<_, i64>(8)? as u64,
                    events_with_partial_usage: row.get::<_, i64>(9)? as u64,
                    late_events: row.get::<_, i64>(10)? as u64,
                    corrections_applied: row.get::<_, i64>(11)? as u64,
                    first_occurred_at: row.get(12)?,
                    last_occurred_at: row.get(13)?,
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

        let detail_from = self.detail_from()?;
        // A window that starts before the retention cutoff is partly answered by
        // rows that no longer exist. Saying so is the point: the alternative is a
        // total that looks like a quiet month.
        let detail_incomplete = match (detail_from, since) {
            (Some(from), Some(since)) => since < from,
            (Some(_), None) => true,
            (None, _) => false,
        };

        Ok(Report {
            since,
            until,
            scope_uid: match scope {
                Scope::Own(uid) => Some(uid),
                Scope::Admin { only_uid } => only_uid,
            },
            principals,
            superseded_by_corrections: superseded as u64,
            detail_from,
            detail_incomplete,
            basis: VISIBILITY_LABEL.to_string(),
            quota_note: SHARED_QUOTA_LABEL.to_string(),
        })
    }

    /// Earliest `occurred_at` still held as detail, if retention has run.
    pub fn detail_from(&self) -> Result<Option<i64>, rusqlite::Error> {
        self.conn
            .query_row(
                "SELECT detail_from FROM retention_state WHERE singleton = 0",
                [],
                |row| row.get(0),
            )
            .optional()
            .map(Option::flatten)
    }
}

/// A recorded boundary in one uid's history.
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct Retirement {
    pub peer_uid: u32,
    /// Everything this uid recorded at or before this instant belongs to the
    /// generation that ended here.
    pub retired_at: i64,
    /// Who held the uid when it was retired, for a human reading the log later.
    pub retired_username: Option<String>,
    pub note: Option<String>,
    pub recorded_at: i64,
    /// Generation this boundary closed.
    pub generation: u32,
}

/// Why a retirement was not recorded.
#[derive(Debug)]
pub enum RetireError {
    /// A boundary already exists at this instant for this uid. Recording the
    /// same retirement twice would invent a generation nobody used.
    AlreadyRecorded {
        peer_uid: u32,
        retired_at: i64,
    },
    Sql(rusqlite::Error),
}

impl std::fmt::Display for RetireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RetireError::AlreadyRecorded {
                peer_uid,
                retired_at,
            } => write!(
                f,
                "uid {peer_uid} already has a retirement recorded at {retired_at}"
            ),
            RetireError::Sql(error) => write!(f, "database error: {error}"),
        }
    }
}

impl std::error::Error for RetireError {}

impl From<rusqlite::Error> for RetireError {
    fn from(error: rusqlite::Error) -> Self {
        RetireError::Sql(error)
    }
}

impl Store {
    /// Records that a uid's account has been removed, closing its generation.
    ///
    /// This is the operator's half of the uid-reuse contract: the kernel will
    /// hand the number out again, and nothing in the database can detect that on
    /// its own, because a uid is all `SO_PEERCRED` reports. Recording the
    /// boundary is what keeps the next holder's usage separate from this one's.
    ///
    /// Deliberately not automatic. Nothing here watches `/etc/passwd`, because a
    /// missing passwd entry is not proof an account was removed — it is also what
    /// a mounted-elsewhere home directory or a directory service outage looks
    /// like, and guessing wrong would split one person's history in two.
    pub fn retire_principal(
        &mut self,
        peer_uid: u32,
        retired_username: Option<&str>,
        note: Option<&str>,
        retired_at: i64,
    ) -> Result<Retirement, RetireError> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let already: Option<i64> = tx
            .query_row(
                "SELECT 1 FROM principal_retirements WHERE peer_uid = ?1 AND retired_at = ?2",
                params![peer_uid, retired_at],
                |row| row.get(0),
            )
            .optional()?;
        if already.is_some() {
            return Err(RetireError::AlreadyRecorded {
                peer_uid,
                retired_at,
            });
        }
        let generation = generation_now(&tx, peer_uid)?;
        let recorded_at = now_secs();
        tx.execute(
            "INSERT INTO principal_retirements \
             (peer_uid, retired_at, retired_username, note, recorded_at) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![peer_uid, retired_at, retired_username, note, recorded_at],
        )?;
        tx.commit()?;
        Ok(Retirement {
            peer_uid,
            retired_at,
            retired_username: retired_username.map(str::to_string),
            note: note.map(str::to_string),
            recorded_at,
            generation,
        })
    }

    /// Every recorded boundary, oldest first.
    pub fn retirements(&self, only_uid: Option<u32>) -> Result<Vec<Retirement>, rusqlite::Error> {
        let mut statement = self.conn.prepare(
            "SELECT peer_uid, retired_at, retired_username, note, recorded_at, \
                    (SELECT COUNT(*) FROM principal_retirements e \
                      WHERE e.peer_uid = r.peer_uid AND e.retired_at < r.retired_at) \
             FROM principal_retirements r \
             WHERE (?1 IS NULL OR peer_uid = ?1) \
             ORDER BY peer_uid, retired_at",
        )?;
        statement
            .query_map(params![only_uid], |row| {
                Ok(Retirement {
                    peer_uid: row.get::<_, i64>(0)? as u32,
                    retired_at: row.get(1)?,
                    retired_username: row.get(2)?,
                    note: row.get(3)?,
                    recorded_at: row.get(4)?,
                    generation: row.get::<_, i64>(5)? as u32,
                })
            })?
            .collect()
    }

    /// The current generation of a uid.
    pub fn current_generation(&self, peer_uid: u32) -> Result<u32, rusqlite::Error> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM principal_retirements WHERE peer_uid = ?1",
            params![peer_uid],
            |row| row.get(0),
        )?;
        Ok(count as u32)
    }
}

/// One account's rolled-up total for one UTC month.
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct MonthlyTotals {
    pub peer_uid: u32,
    pub principal_generation: u32,
    /// `YYYY-MM`, UTC.
    pub month: String,
    pub peer_username: Option<String>,
    pub events: u64,
    pub total_tokens: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub events_with_unknown_usage: u64,
    pub events_with_partial_usage: u64,
    pub first_occurred_at: Option<i64>,
    pub last_occurred_at: Option<i64>,
    pub rolled_up_at: i64,
}

impl Store {
    /// Rolled-up months the caller may see.
    ///
    /// These outlive the detail they were built from, which is the point: a
    /// monthly total answers what an account cost without keeping a record of
    /// each individual thing that account ran.
    pub fn monthly(
        &self,
        scope: Scope,
        from_month: Option<&str>,
        to_month: Option<&str>,
    ) -> Result<Vec<MonthlyTotals>, rusqlite::Error> {
        let (uid_clause, uid_param) = scope_clause(scope, "");
        let sql = format!(
            "SELECT peer_uid, principal_generation, month, peer_username, events, \
                    total_tokens, input_tokens, output_tokens, \
                    events_with_unknown_usage, events_with_partial_usage, \
                    first_occurred_at, last_occurred_at, rolled_up_at \
             FROM usage_monthly \
             WHERE 1 = 1 {uid_clause} \
               AND (?2 IS NULL OR month >= ?2) \
               AND (?3 IS NULL OR month <= ?3) \
             ORDER BY peer_uid, principal_generation, month"
        );
        let mut statement = self.conn.prepare(&sql)?;
        statement
            .query_map(params![uid_param, from_month, to_month], |row| {
                Ok(MonthlyTotals {
                    peer_uid: row.get::<_, i64>(0)? as u32,
                    principal_generation: row.get::<_, i64>(1)? as u32,
                    month: row.get(2)?,
                    peer_username: row.get(3)?,
                    events: row.get::<_, i64>(4)? as u64,
                    total_tokens: row.get::<_, i64>(5)? as u64,
                    input_tokens: row.get::<_, i64>(6)? as u64,
                    output_tokens: row.get::<_, i64>(7)? as u64,
                    events_with_unknown_usage: row.get::<_, i64>(8)? as u64,
                    events_with_partial_usage: row.get::<_, i64>(9)? as u64,
                    first_occurred_at: row.get(10)?,
                    last_occurred_at: row.get(11)?,
                    rolled_up_at: row.get(12)?,
                })
            })?
            .collect()
    }
}

/// A published backup.
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct BackupRecord {
    pub path: PathBuf,
    pub bytes: u64,
    /// Rows in the snapshot.
    pub events: u64,
    /// Highest row id the snapshot contains. This is the recovery point:
    /// restoring returns the database to exactly this state and no further.
    pub recovery_point_id: Option<i64>,
    pub taken_at: i64,
    /// Older backups removed to honour the retention bound.
    pub pruned: Vec<PathBuf>,
}

/// Why a backup was not published.
#[derive(Debug)]
pub enum BackupError {
    /// The copy was made but did not verify, so nothing was published.
    Unverified(String),
    Sql(rusqlite::Error),
    Io(PathBuf, std::io::Error),
}

impl std::fmt::Display for BackupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // Neutral wording: this is raised both when a fresh copy is
            // rejected and when an existing file is refused as a restore source,
            // and "discarded" would be wrong in the second case.
            BackupError::Unverified(why) => write!(f, "the snapshot did not verify: {why}"),
            BackupError::Sql(error) => write!(f, "database error: {error}"),
            BackupError::Io(path, error) => write!(f, "{}: {error}", path.display()),
        }
    }
}

impl std::error::Error for BackupError {}

impl From<rusqlite::Error> for BackupError {
    fn from(error: rusqlite::Error) -> Self {
        BackupError::Sql(error)
    }
}

/// Prefix of a copy that is not yet a backup.
const PARTIAL_PREFIX: &str = ".partial-";

impl Store {
    /// Takes a verified snapshot and publishes it under a dated name.
    ///
    /// The copy is written to a temporary name, verified, and only then renamed.
    /// A run that dies part way through therefore leaves something that is
    /// visibly not a backup, rather than a short file that looks like one.
    ///
    /// `VACUUM INTO` takes a read transaction, so this is safe while the daemon
    /// is writing. The snapshot is consistent as of some instant during the
    /// copy; it is not a promise about anything acknowledged afterwards.
    pub fn backup(&self, into: &Path, keep: usize) -> Result<BackupRecord, BackupError> {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        if !into.exists() {
            builder
                .create(into)
                .map_err(|error| BackupError::Io(into.to_path_buf(), error))?;
        }

        // Everything committed before the copy starts must be in it. Recorded
        // first so the check cannot be satisfied by a snapshot that lost rows.
        let committed_before: Option<i64> = self
            .conn
            .query_row("SELECT MAX(id) FROM usage_events", [], |row| row.get(0))
            .optional()?
            .flatten();

        let taken_at = now_secs();
        let temp = into.join(format!(
            "{PARTIAL_PREFIX}{}-{}.db",
            std::process::id(),
            taken_at
        ));
        let _ = std::fs::remove_file(&temp);
        self.conn
            .execute("VACUUM INTO ?1", params![temp.to_string_lossy()])?;
        restrict(&temp)?;

        match verify(&temp, committed_before) {
            Ok((events, recovery_point_id)) => {
                let published = into.join(format!("usage-{}.db", stamp(taken_at)));
                std::fs::rename(&temp, &published)
                    .map_err(|error| BackupError::Io(temp.clone(), error))?;
                let bytes = std::fs::metadata(&published)
                    .map(|meta| meta.len())
                    .unwrap_or_default();
                Ok(BackupRecord {
                    path: published,
                    bytes,
                    events,
                    recovery_point_id,
                    taken_at,
                    pruned: prune(into, keep)?,
                })
            }
            Err(why) => {
                // Deleted, not published. A backup nobody can restore is worse
                // than an obvious absence, because it is trusted.
                let _ = std::fs::remove_file(&temp);
                Err(BackupError::Unverified(why))
            }
        }
    }
}

/// Published backups, newest last.
pub fn list_backups(dir: &Path) -> Result<Vec<PathBuf>, BackupError> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|error| BackupError::Io(dir.to_path_buf(), error))?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| is_published(path))
        .collect();
    paths.sort();
    Ok(paths)
}

/// True for a name this module publishes, and false for a copy in progress.
fn is_published(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            name.starts_with("usage-") && name.ends_with(".db") && !name.starts_with(PARTIAL_PREFIX)
        })
}

/// Checks a copy before it is allowed to become a backup.
///
/// Integrity alone is not enough: an empty but structurally valid database
/// passes it. The dedup key has to be present or a restored database would
/// accept a replay as a new event, and every row committed before the copy
/// began has to be there or the snapshot silently lost usage.
fn verify(path: &Path, committed_before: Option<i64>) -> Result<(u64, Option<i64>), String> {
    let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|error| format!("it could not be opened: {error}"))?;

    let integrity: String = conn
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .map_err(|error| format!("integrity_check failed to run: {error}"))?;
    if integrity != "ok" {
        return Err(format!("integrity_check said {integrity:?}"));
    }

    let dedup_key: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type = 'index' AND tbl_name = 'usage_events' AND sql IS NULL",
            [],
            |row| row.get(0),
        )
        .map_err(|error| format!("the schema could not be read: {error}"))?;
    if dedup_key == 0 {
        return Err("the (peer_uid, client_event_id) unique key is missing".to_string());
    }

    let events: i64 = conn
        .query_row("SELECT COUNT(*) FROM usage_events", [], |row| row.get(0))
        .map_err(|error| format!("usage_events could not be read: {error}"))?;
    let newest: Option<i64> = conn
        .query_row("SELECT MAX(id) FROM usage_events", [], |row| row.get(0))
        .map_err(|error| format!("usage_events could not be read: {error}"))?;

    if let Some(expected) = committed_before {
        let present: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM usage_events WHERE id <= ?1",
                params![expected],
                |row| row.get(0),
            )
            .map_err(|error| format!("usage_events could not be read: {error}"))?;
        if present == 0 && expected > 0 {
            return Err("it contains none of the rows committed before the copy".to_string());
        }
        if newest.unwrap_or(0) < expected {
            return Err(format!(
                "it stops at row {} but row {expected} was committed before the copy began",
                newest.unwrap_or(0)
            ));
        }
    }
    Ok((events as u64, newest))
}

/// Keeps the newest `keep` backups and removes the rest.
fn prune(dir: &Path, keep: usize) -> Result<Vec<PathBuf>, BackupError> {
    let mut published = list_backups(dir)?;
    if published.len() <= keep {
        return Ok(Vec::new());
    }
    let remove = published.len() - keep;
    let mut pruned = Vec::new();
    for path in published.drain(..remove) {
        std::fs::remove_file(&path).map_err(|error| BackupError::Io(path.clone(), error))?;
        pruned.push(path);
    }
    Ok(pruned)
}

/// What restoring a backup would give you.
#[derive(Clone, Debug, serde::Serialize)]
pub struct RestorePlan {
    pub from: PathBuf,
    pub to: PathBuf,
    pub events: u64,
    /// The state the database would be returned to. Anything acknowledged after
    /// this row is not in the backup and does not come back.
    pub recovery_point_id: Option<i64>,
    /// Rows currently in the live database that the backup does not have.
    pub live_rows_not_in_backup: Option<u64>,
    pub caveat: String,
}

/// What a restore is, said plainly wherever one is offered.
pub const RESTORE_CAVEAT: &str = "Restoring is a recovery-point rollback, not a repair: the \
database is returned to the state in the backup, and any event acknowledged after that point is \
gone. It is not evidence that later acknowledged events survived.";

/// Checks a backup and describes what restoring it would do.
///
/// Deliberately separate from performing the restore. The number worth seeing
/// before overwriting anything is how much live data the backup does not have.
pub fn plan_restore(from: &Path, to: &Path) -> Result<RestorePlan, BackupError> {
    let (events, recovery_point_id) = verify(from, None).map_err(BackupError::Unverified)?;

    let live_rows_not_in_backup = match (to.exists(), recovery_point_id) {
        (true, Some(point)) => {
            Connection::open_with_flags(to, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .ok()
                .and_then(|conn| {
                    conn.query_row(
                        "SELECT COUNT(*) FROM usage_events WHERE id > ?1",
                        params![point],
                        |row| row.get::<_, i64>(0),
                    )
                    .ok()
                })
                .map(|count| count as u64)
        }
        _ => None,
    };

    Ok(RestorePlan {
        from: from.to_path_buf(),
        to: to.to_path_buf(),
        events,
        recovery_point_id,
        live_rows_not_in_backup,
        caveat: RESTORE_CAVEAT.to_string(),
    })
}

/// Puts a verified backup in place, keeping the database it replaced.
pub fn restore(from: &Path, to: &Path) -> Result<RestorePlan, BackupError> {
    let plan = plan_restore(from, to)?;
    if to.exists() {
        // Never discard the live database on the strength of a rollback. Its WAL
        // is left alone: SQLite discards a WAL whose database has been replaced.
        let aside = to.with_extension(format!("replaced-{}", stamp(now_secs())));
        std::fs::rename(to, &aside).map_err(|error| BackupError::Io(to.to_path_buf(), error))?;
    }
    std::fs::copy(from, to).map_err(|error| BackupError::Io(to.to_path_buf(), error))?;
    restrict(to)?;
    Ok(plan)
}

/// Keeps a file readable only by its owner.
fn restrict(path: &Path) -> Result<(), BackupError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|error| BackupError::Io(path.to_path_buf(), error))?;
    }
    Ok(())
}

/// A sortable UTC stamp, so lexical order is chronological order.
fn stamp(seconds: i64) -> String {
    crate::recorder::render::utc(seconds).replace([':', '-'], "")
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or_default()
}
