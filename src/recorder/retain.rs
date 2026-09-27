//! Retention by rebuild: age detail out, keep monthly totals.
//!
//! `usage_events` is append-only, and that is the basis on which a report from
//! last month can be trusted this month. Retention therefore does not delete
//! rows — there is no code path that can, and the triggers would refuse it. It
//! builds a new database holding the detail still inside the window plus a
//! rolled-up total for everything older, verifies that not one token went
//! missing across the boundary, and swaps the file.
//!
//! So each published database is itself append-only for its whole life. History
//! is reshaped only at a visible, verified, operator-initiated boundary, and the
//! previous file is kept rather than removed.
//!
//! Aggregates are the point of the exercise. A monthly total answers what an
//! account cost; a year of event rows also records each individual thing that
//! account ran, which is a different and more sensitive thing to keep.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, params};

/// Detail kept by default, in days.
///
/// Long enough to answer "what happened last quarter" and to investigate a
/// disputed figure; short enough that a shared host is not accumulating a
/// permanent per-person record of everything anyone ran.
pub const DEFAULT_KEEP_DAYS: u32 = 180;

/// What retention would do, or did.
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct RetentionPlan {
    pub keep_days: u32,
    /// Detail at or after this instant stays as individual events.
    pub cutoff: i64,
    /// Rows that would stop being individual events.
    pub events_rolled_up: u64,
    /// Of those, the ones that count toward a total. The rest are rows a
    /// correction already replaced, which were never counted either.
    pub live_events_rolled_up: u64,
    /// Tokens moved from detail into monthly totals. Not lost: still reported,
    /// just no longer attributable to one attempt.
    pub tokens_rolled_up: u64,
    /// Months that gained or grew a rolled-up total.
    pub months: Vec<String>,
    /// Rows older than the cutoff that stay as detail anyway, because a
    /// correction links them to something inside the window. Keeping a
    /// correction chain whole is what stops a correction from pointing at a row
    /// that no longer exists.
    pub held_back_by_corrections: u64,
    /// The boundary below which detail is no longer complete. This is the
    /// cutoff, not the earliest surviving row: a row's age says when someone
    /// happened to run something, which is not a statement about coverage.
    ///
    /// Monotonic across runs. A later pass with a *longer* window must not claim
    /// coverage that an earlier, shorter one already removed.
    pub detail_from: i64,
    /// Earliest event still held as detail, which can predate `detail_from` when
    /// a correction chain was kept whole.
    pub earliest_detail: Option<i64>,
}

/// A completed rebuild.
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct RetentionReport {
    pub plan: RetentionPlan,
    pub database: PathBuf,
    /// The pre-retention file, kept. Removing it is the operator's call, not
    /// this command's: it is the only copy of the detail that was just rolled
    /// up, and a mistaken `--keep-days` should be recoverable.
    pub previous: PathBuf,
    pub bytes_before: u64,
    pub bytes_after: u64,
}

/// Why a rebuild did not happen.
#[derive(Debug)]
pub enum RetainError {
    /// The daemon is still accepting connections on its socket. It is the only
    /// writer, and swapping the file under it would leave it writing to an inode
    /// nothing reads.
    DaemonLive(PathBuf),
    /// The rebuild did not conserve the record, so nothing was published.
    Unverified(String),
    Sql(rusqlite::Error),
    Io(PathBuf, std::io::Error),
}

impl std::fmt::Display for RetainError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RetainError::DaemonLive(path) => write!(
                f,
                "the recorder is still listening on {}; stop it before retention runs",
                path.display()
            ),
            RetainError::Unverified(why) => {
                write!(f, "the rebuilt database did not verify: {why}")
            }
            RetainError::Sql(error) => write!(f, "database error: {error}"),
            RetainError::Io(path, error) => write!(f, "{}: {error}", path.display()),
        }
    }
}

impl std::error::Error for RetainError {}

impl From<rusqlite::Error> for RetainError {
    fn from(error: rusqlite::Error) -> Self {
        RetainError::Sql(error)
    }
}

/// True when something is accepting connections on `socket`.
///
/// A stale socket file left by a killed daemon is not a live daemon, so the
/// check is a connection attempt rather than an existence test.
#[cfg(unix)]
pub fn daemon_is_live(socket: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(socket).is_ok()
}

#[cfg(not(unix))]
pub fn daemon_is_live(_socket: &Path) -> bool {
    false
}

/// Rolls detail older than `keep_days` into monthly totals.
///
/// `socket`, when given, is probed first and the rebuild refused if the daemon
/// answers.
pub fn retain(
    database: &Path,
    keep_days: u32,
    now: i64,
    socket: Option<&Path>,
    dry_run: bool,
) -> Result<RetentionReport, RetainError> {
    if let Some(socket) = socket
        && daemon_is_live(socket)
    {
        return Err(RetainError::DaemonLive(socket.to_path_buf()));
    }

    let cutoff = now.saturating_sub(i64::from(keep_days).saturating_mul(86_400));
    let bytes_before = std::fs::metadata(database)
        .map(|meta| meta.len())
        .unwrap_or_default();

    let parent = database.parent().unwrap_or(Path::new("."));
    let temp = parent.join(format!(".partial-retain-{}-{}.db", std::process::id(), now));
    let _ = std::fs::remove_file(&temp);

    let outcome = build(database, &temp, keep_days, cutoff, now);
    let plan = match outcome {
        Ok(plan) => plan,
        Err(error) => {
            let _ = std::fs::remove_file(&temp);
            return Err(error);
        }
    };

    if dry_run {
        let _ = std::fs::remove_file(&temp);
        return Ok(RetentionReport {
            plan,
            database: database.to_path_buf(),
            previous: PathBuf::new(),
            bytes_before,
            bytes_after: 0,
        });
    }

    let bytes_after = std::fs::metadata(&temp)
        .map(|meta| meta.len())
        .unwrap_or_default();
    let previous = parent.join(format!(
        "{}.pre-retain-{}.db",
        database
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| "usage.db".to_string()),
        now
    ));

    // The old file is moved aside before the new one takes its name, so a crash
    // between the two leaves both files present under distinguishable names
    // rather than none.
    std::fs::rename(database, &previous)
        .map_err(|error| RetainError::Io(database.into(), error))?;
    if let Err(error) = std::fs::rename(&temp, database) {
        // Put it back. Failing with the database missing would be far worse than
        // failing with retention not done.
        let _ = std::fs::rename(&previous, database);
        return Err(RetainError::Io(temp, error));
    }
    sync_dir(parent)?;

    Ok(RetentionReport {
        plan,
        database: database.to_path_buf(),
        previous,
        bytes_before,
        bytes_after,
    })
}

/// Builds the rebuilt database at `temp` and returns what it did.
fn build(
    live: &Path,
    temp: &Path,
    keep_days: u32,
    cutoff: i64,
    now: i64,
) -> Result<RetentionPlan, RetainError> {
    // Schema first, through the ordinary path, so the rebuilt file is byte-for-
    // byte the shape a fresh install would have — including the append-only
    // triggers, which permit the inserts below and nothing after them.
    {
        let store = crate::recorder::db::Store::open(temp)?;
        drop(store);
    }

    let conn = Connection::open(temp)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "FULL")?;
    conn.execute_batch(&format!(
        "ATTACH DATABASE '{}' AS live;",
        live.to_string_lossy().replace('\'', "''")
    ))?;

    // Rows to keep as detail: everything at or after the cutoff, plus the whole
    // correction chain of anything kept. Walking the `corrects` edges in both
    // directions keeps a chain together, so no retained correction can end up
    // pointing at a row that was rolled up.
    conn.execute_batch(
        r#"
        CREATE TEMP TABLE kept_ids (id INTEGER PRIMARY KEY);
        "#,
    )?;
    conn.execute(
        r#"
        INSERT INTO kept_ids (id)
        WITH RECURSIVE chain(id) AS (
            SELECT id FROM live.usage_events WHERE occurred_at >= ?1
            UNION
            SELECT p.id FROM live.usage_events p, chain c
              WHERE p.id = (SELECT corrects FROM live.usage_events WHERE id = c.id)
            UNION
            SELECT n.id FROM live.usage_events n, chain c WHERE n.corrects = c.id
        )
        SELECT id FROM chain
        "#,
        params![cutoff],
    )?;

    let held_back: i64 = conn.query_row(
        "SELECT COUNT(*) FROM live.usage_events \
         WHERE occurred_at < ?1 AND id IN (SELECT id FROM kept_ids)",
        params![cutoff],
        |row| row.get(0),
    )?;

    // Everything else is rolled up. A row a correction replaced is rolled up
    // too, but not counted: it was never counted by a report either.
    conn.execute_batch(
        r#"
        CREATE TEMP TABLE rolled AS
        SELECT e.*,
               NOT EXISTS (
                   SELECT 1 FROM live.usage_events c WHERE c.corrects = e.id
               ) AS is_live
        FROM live.usage_events e
        WHERE e.id NOT IN (SELECT id FROM kept_ids);
        "#,
    )?;

    let (events_rolled_up, live_events_rolled_up, tokens_rolled_up): (i64, i64, i64) = conn
        .query_row(
            "SELECT COUNT(*), COALESCE(SUM(is_live), 0), \
                    COALESCE(SUM(CASE WHEN is_live THEN total_tokens ELSE 0 END), 0) FROM rolled",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;

    // Detail, ids carried over so `corrects` still resolves.
    conn.execute_batch(
        r#"
        INSERT INTO main.usage_events (
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
            corrects, client_payload, principal_generation
        FROM live.usage_events
        WHERE id IN (SELECT id FROM kept_ids)
        ORDER BY id;

        -- Boundaries are carried over in full. Losing one would silently merge
        -- two generations of a uid the next time anything was totalled.
        INSERT INTO main.principal_retirements
            (peer_uid, retired_at, retired_username, note, recorded_at)
        SELECT peer_uid, retired_at, retired_username, note, recorded_at
        FROM live.principal_retirements;
        "#,
    )?;

    // New rollups and any earlier ones, summed per account, generation and
    // month. Earlier aggregates are carried forward, so running retention twice
    // adds to a month rather than replacing it.
    conn.execute(
        r#"
        CREATE TEMP TABLE combined AS
        SELECT peer_uid, principal_generation,
               strftime('%Y-%m', occurred_at, 'unixepoch') AS month,
               peer_username,
               1 AS events,
               COALESCE(total_tokens, 0) AS total_tokens,
               COALESCE(input_tokens, 0) AS input_tokens,
               COALESCE(output_tokens, 0) AS output_tokens,
               (total_tokens IS NULL) AS unknown_usage,
               (usage_status = 'partial') AS partial_usage,
               occurred_at AS first_occurred_at,
               occurred_at AS last_occurred_at
        FROM rolled
        WHERE is_live
        UNION ALL
        SELECT peer_uid, principal_generation, month, peer_username,
               events, total_tokens, input_tokens, output_tokens,
               events_with_unknown_usage, events_with_partial_usage,
               first_occurred_at, last_occurred_at
        FROM live.usage_monthly
        "#,
        [],
    )?;

    conn.execute(
        r#"
        INSERT INTO main.usage_monthly (
            peer_uid, principal_generation, month, peer_username, events,
            total_tokens, input_tokens, output_tokens,
            events_with_unknown_usage, events_with_partial_usage,
            first_occurred_at, last_occurred_at, rolled_up_at
        )
        SELECT peer_uid, principal_generation, month, NULL,
               SUM(events), SUM(total_tokens), SUM(input_tokens), SUM(output_tokens),
               SUM(unknown_usage), SUM(partial_usage),
               MIN(first_occurred_at), MAX(last_occurred_at), ?1
        FROM combined
        GROUP BY peer_uid, principal_generation, month
        "#,
        params![now],
    )?;

    // Name filled in afterwards rather than inside the aggregate, so it is
    // unambiguously the latest name seen in that month rather than whichever row
    // the grouping happened to surface.
    conn.execute(
        r#"
        UPDATE main.usage_monthly SET peer_username = (
            SELECT c.peer_username FROM combined c
            WHERE c.peer_uid = main.usage_monthly.peer_uid
              AND c.principal_generation = main.usage_monthly.principal_generation
              AND c.month = main.usage_monthly.month
              AND c.peer_username IS NOT NULL
            ORDER BY c.last_occurred_at DESC LIMIT 1
        )
        "#,
        [],
    )?;

    let earliest_detail: Option<i64> = conn
        .query_row(
            "SELECT MIN(occurred_at) FROM main.usage_events",
            [],
            |row| row.get(0),
        )
        .optional()?
        .flatten();

    // Never moves backwards. Running retention again with a longer window cannot
    // restore detail an earlier, shorter window already rolled up, so claiming
    // the earlier boundary would overstate what the database holds.
    let previous_boundary: Option<i64> = conn
        .query_row(
            "SELECT detail_from FROM live.retention_state WHERE singleton = 0",
            [],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    let detail_from = previous_boundary.map_or(cutoff, |earlier| earlier.max(cutoff));

    let rows_rolled_before: i64 = conn
        .query_row(
            "SELECT rows_rolled FROM live.retention_state WHERE singleton = 0",
            [],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or(0);

    // Recorded so a report covering a pruned stretch can say so rather than
    // returning a total that reads like a quiet month.
    conn.execute(
        "INSERT INTO main.retention_state (singleton, detail_from, keep_days, last_run_at, rows_rolled) \
         VALUES (0, ?1, ?2, ?3, ?4)",
        params![
            detail_from,
            keep_days,
            now,
            rows_rolled_before + events_rolled_up
        ],
    )?;

    let months: Vec<String> = {
        let mut statement = conn.prepare("SELECT DISTINCT month FROM combined ORDER BY month")?;
        statement
            .query_map([], |row| row.get(0))?
            .collect::<Result<Vec<String>, _>>()?
    };

    verify(&conn)?;
    conn.execute_batch("DETACH DATABASE live;")?;
    conn.pragma_update(None, "wal_checkpoint", "TRUNCATE")?;
    drop(conn);
    sync_file(temp)?;

    Ok(RetentionPlan {
        keep_days,
        cutoff,
        events_rolled_up: events_rolled_up as u64,
        live_events_rolled_up: live_events_rolled_up as u64,
        tokens_rolled_up: tokens_rolled_up as u64,
        months,
        held_back_by_corrections: held_back as u64,
        detail_from,
        earliest_detail,
    })
}

/// Checks that the rebuild conserved what a report would have counted.
///
/// The invariant is the one that matters to a reader: live events and their
/// tokens, summed across detail and monthly totals, must be identical before and
/// after. A rebuild that lost a row, or double-counted one by rolling it up and
/// keeping it, fails here and is never published.
fn verify(conn: &Connection) -> Result<(), RetainError> {
    let before: (i64, i64) = conn.query_row(
        r#"
        SELECT
          (SELECT COUNT(*) FROM live.usage_events e
            WHERE NOT EXISTS (SELECT 1 FROM live.usage_events c WHERE c.corrects = e.id))
          + (SELECT COALESCE(SUM(events), 0) FROM live.usage_monthly),
          (SELECT COALESCE(SUM(e.total_tokens), 0) FROM live.usage_events e
            WHERE NOT EXISTS (SELECT 1 FROM live.usage_events c WHERE c.corrects = e.id))
          + (SELECT COALESCE(SUM(total_tokens), 0) FROM live.usage_monthly)
        "#,
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let after: (i64, i64) = conn.query_row(
        r#"
        SELECT
          (SELECT COUNT(*) FROM main.usage_events e
            WHERE NOT EXISTS (SELECT 1 FROM main.usage_events c WHERE c.corrects = e.id))
          + (SELECT COALESCE(SUM(events), 0) FROM main.usage_monthly),
          (SELECT COALESCE(SUM(e.total_tokens), 0) FROM main.usage_events e
            WHERE NOT EXISTS (SELECT 1 FROM main.usage_events c WHERE c.corrects = e.id))
          + (SELECT COALESCE(SUM(total_tokens), 0) FROM main.usage_monthly)
        "#,
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if before != after {
        return Err(RetainError::Unverified(format!(
            "{} live events and {} tokens before, {} and {} after",
            before.0, before.1, after.0, after.1
        )));
    }

    // A retained correction pointing at a rolled-up row would be a dangling
    // reference. The chain closure above is meant to make this impossible; this
    // is the check that the closure actually held.
    let dangling: i64 = conn.query_row(
        "SELECT COUNT(*) FROM main.usage_events e WHERE e.corrects IS NOT NULL \
         AND NOT EXISTS (SELECT 1 FROM main.usage_events t WHERE t.id = e.corrects)",
        [],
        |row| row.get(0),
    )?;
    if dangling != 0 {
        return Err(RetainError::Unverified(format!(
            "{dangling} retained correction(s) point at a row that was rolled up"
        )));
    }

    let mut check = conn.prepare("PRAGMA main.foreign_key_check")?;
    let violations: i64 = check.query_map([], |_| Ok(()))?.count() as i64;
    if violations != 0 {
        return Err(RetainError::Unverified(format!(
            "{violations} foreign key violation(s) in the rebuilt database"
        )));
    }
    Ok(())
}

fn sync_file(path: &Path) -> Result<(), RetainError> {
    let file = std::fs::File::open(path).map_err(|e| RetainError::Io(path.into(), e))?;
    file.sync_all().map_err(|e| RetainError::Io(path.into(), e))
}

fn sync_dir(dir: &Path) -> Result<(), RetainError> {
    let file = std::fs::File::open(dir).map_err(|e| RetainError::Io(dir.into(), e))?;
    // A rename is only durable once the directory entry is. Ignored on platforms
    // where a directory cannot be opened for this.
    let _ = file.sync_all();
    Ok(())
}
