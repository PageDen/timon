//! Retention: detail ages out, totals do not, and nothing goes missing.
//!
//! The property every test here is really checking is conservation. Retention
//! reshapes how usage is stored; it must never change what a report says usage
//! was. A rebuild that lost a row, or counted one twice by rolling it up *and*
//! keeping it, would be worse than no retention at all.

use serde_json::json;
use timon::recorder::db::{Scope, Store};
use timon::recorder::event::UsageEvent;
use timon::recorder::retain::{self, RetainError};

const UID: u32 = 5001;
const OTHER: u32 = 5002;
const DAY: i64 = 86_400;
/// 2027-01-15T00:00:00Z, so month boundaries in assertions are readable.
const NOW: i64 = 1_800_000_000;

fn event(value: serde_json::Value) -> UsageEvent {
    serde_json::from_value(value).expect("event should parse")
}

fn at(event_id: &str, occurred_at: i64, tokens: u64) -> UsageEvent {
    event(json!({
        "version": 1,
        "client_event_id": event_id,
        "run_id": event_id,
        "attempt_id": "1",
        "role": "worker",
        "provider": "openai",
        "model": "gpt-5.5",
        "usage": { "input": tokens, "cached_input": 0, "output": 0, "reasoning_output": 0 },
        "usage_status": "complete",
        "duration_ms": 10,
        "occurred_at": occurred_at
    }))
}

/// A database with one row well inside the window and one well outside it.
fn seeded(path: &std::path::Path) -> (i64, i64) {
    let mut store = Store::open(path).unwrap();
    let old_at = NOW - 300 * DAY;
    let new_at = NOW - 10 * DAY;
    store
        .append(UID, Some("alice"), &at("old", old_at, 1_000), old_at + 1)
        .unwrap();
    store
        .append(UID, Some("alice"), &at("new", new_at, 7), new_at + 1)
        .unwrap();
    (old_at, new_at)
}

#[test]
fn detail_older_than_the_window_becomes_a_monthly_total() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("usage.db");
    seeded(&path);

    let report = retain::retain(&path, 180, NOW, None, false).unwrap();
    assert_eq!(report.plan.events_rolled_up, 1);
    assert_eq!(report.plan.live_events_rolled_up, 1);
    assert_eq!(report.plan.tokens_rolled_up, 1_000);

    let store = Store::open(&path).unwrap();
    let rows = store.query(Scope::Own(UID), None, None, 100).unwrap();
    assert_eq!(rows.len(), 1, "only the recent event is still an event");
    assert_eq!(rows[0].client_event_id, "new");

    let monthly = store.monthly(Scope::Own(UID), None, None).unwrap();
    assert_eq!(monthly.len(), 1, "the old month is kept as a total");
    assert_eq!(monthly[0].events, 1);
    assert_eq!(monthly[0].total_tokens, 1_000);
    assert_eq!(monthly[0].peer_username.as_deref(), Some("alice"));
    assert_eq!(monthly[0].month.len(), 7, "YYYY-MM");
}

#[test]
fn a_report_covering_a_pruned_stretch_says_the_detail_is_gone() {
    // The failure this prevents is the quiet one: a window reaching back before
    // the cutoff returning a small total that reads like a quiet month, when it
    // is really a month whose detail was rolled up.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("usage.db");
    let (old_at, _) = seeded(&path);
    retain::retain(&path, 180, NOW, None, false).unwrap();

    let store = Store::open(&path).unwrap();
    let narrow = store
        .report(Scope::Own(UID), Some(NOW - 30 * DAY), Some(NOW))
        .unwrap();
    assert!(
        !narrow.detail_incomplete,
        "a window entirely inside the retained range is complete"
    );

    let wide = store
        .report(Scope::Own(UID), Some(old_at), Some(NOW))
        .unwrap();
    assert!(
        wide.detail_incomplete,
        "a window reaching past the cutoff must say so"
    );
    assert!(wide.detail_from.is_some());

    let unbounded = store.report(Scope::Own(UID), None, None).unwrap();
    assert!(
        unbounded.detail_incomplete,
        "an open-ended window reaches past the cutoff by definition"
    );
}

#[test]
fn a_correction_chain_crossing_the_cutoff_is_kept_whole() {
    // A retained correction pointing at a rolled-up row would dangle. Keeping the
    // chain together is the fix; this checks it holds even though the corrected
    // row is old enough to be pruned.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("usage.db");
    let old_at = NOW - 300 * DAY;
    let corrected_id = {
        let mut store = Store::open(&path).unwrap();
        let first = store
            .append(UID, Some("alice"), &at("old", old_at, 1_000), old_at + 1)
            .unwrap();
        let mut correction = json!({
            "version": 1,
            "client_event_id": "fix",
            "run_id": "old",
            "attempt_id": "1",
            "role": "worker",
            "usage": { "input": 1_500, "cached_input": 0, "output": 0, "reasoning_output": 0 },
            "usage_status": "complete",
            "occurred_at": NOW - 5 * DAY
        });
        correction["corrects"] = json!(first.id);
        store
            .append(UID, Some("alice"), &event(correction), NOW - 5 * DAY + 1)
            .unwrap();
        first.id
    };

    let report = retain::retain(&path, 180, NOW, None, false).unwrap();
    assert_eq!(
        report.plan.held_back_by_corrections, 1,
        "the old row stays because a recent correction points at it"
    );
    assert_eq!(report.plan.events_rolled_up, 0);

    let store = Store::open(&path).unwrap();
    let rows = store.query(Scope::Own(UID), None, None, 100).unwrap();
    assert_eq!(rows.len(), 2, "both halves of the chain are still detail");
    assert!(
        rows.iter().any(|row| row.id == corrected_id),
        "the corrected row survived, so the correction still resolves"
    );
    // The correction replaces the original rather than adding to it.
    let totals = store.report(Scope::Own(UID), None, None).unwrap();
    assert_eq!(totals.principals[0].total_tokens, 1_500);
}

#[test]
fn a_superseded_row_is_not_counted_when_its_whole_chain_ages_out() {
    // Both halves are old, so both are rolled up. The rolled-up total must be the
    // correction's figure alone, exactly as a report would have counted it.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("usage.db");
    let old_at = NOW - 300 * DAY;
    {
        let mut store = Store::open(&path).unwrap();
        let first = store
            .append(UID, Some("alice"), &at("old", old_at, 1_000), old_at + 1)
            .unwrap();
        let mut correction = json!({
            "version": 1,
            "client_event_id": "fix",
            "run_id": "old",
            "attempt_id": "1",
            "role": "worker",
            "usage": { "input": 1_500, "cached_input": 0, "output": 0, "reasoning_output": 0 },
            "usage_status": "complete",
            "occurred_at": old_at + 60
        });
        correction["corrects"] = json!(first.id);
        store
            .append(UID, Some("alice"), &event(correction), old_at + 61)
            .unwrap();
    }

    let report = retain::retain(&path, 180, NOW, None, false).unwrap();
    assert_eq!(report.plan.events_rolled_up, 2, "both rows leave detail");
    assert_eq!(
        report.plan.live_events_rolled_up, 1,
        "only the correction was ever counted"
    );
    assert_eq!(report.plan.tokens_rolled_up, 1_500);

    let store = Store::open(&path).unwrap();
    let monthly = store.monthly(Scope::Own(UID), None, None).unwrap();
    assert_eq!(monthly.len(), 1);
    assert_eq!(
        monthly[0].total_tokens, 1_500,
        "the superseded 1,000 is not added to the 1,500 that replaced it"
    );
    assert_eq!(monthly[0].events, 1);
}

#[test]
fn running_retention_twice_adds_to_a_month_rather_than_replacing_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("usage.db");
    let old_at = NOW - 300 * DAY;
    {
        let mut store = Store::open(&path).unwrap();
        store
            .append(UID, Some("alice"), &at("a", old_at, 100), old_at + 1)
            .unwrap();
        store
            .append(UID, Some("alice"), &at("b", old_at + 60, 200), old_at + 61)
            .unwrap();
        // Inside the window on the first pass, outside it on the second.
        store
            .append(
                UID,
                Some("alice"),
                &at("c", NOW - 170 * DAY, 400),
                NOW - 170 * DAY,
            )
            .unwrap();
    }

    retain::retain(&path, 180, NOW, None, false).unwrap();
    let first = {
        let store = Store::open(&path).unwrap();
        store.monthly(Scope::Own(UID), None, None).unwrap()
    };
    assert_eq!(first.iter().map(|m| m.total_tokens).sum::<u64>(), 300);

    // Later, with a shorter window.
    retain::retain(&path, 150, NOW, None, false).unwrap();
    let store = Store::open(&path).unwrap();
    let second = store.monthly(Scope::Own(UID), None, None).unwrap();
    assert_eq!(
        second.iter().map(|m| m.total_tokens).sum::<u64>(),
        700,
        "the earlier rollup is carried forward, not overwritten"
    );
    assert_eq!(second.iter().map(|m| m.events).sum::<u64>(), 3);
    assert_eq!(
        store.query(Scope::Own(UID), None, None, 100).unwrap().len(),
        0,
        "everything is now older than the shorter window"
    );
}

#[test]
fn generations_are_rolled_up_separately() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("usage.db");
    let old_at = NOW - 300 * DAY;
    {
        let mut store = Store::open(&path).unwrap();
        store
            .append(UID, Some("alice"), &at("a", old_at, 100), old_at + 1)
            .unwrap();
        store
            .retire_principal(UID, Some("alice"), None, old_at + 100)
            .unwrap();
        store
            .append(UID, Some("bob"), &at("a", old_at + 200, 7), old_at + 201)
            .unwrap();
    }

    retain::retain(&path, 180, NOW, None, false).unwrap();

    let store = Store::open(&path).unwrap();
    let all = store
        .monthly(
            Scope::Admin {
                only_uid: Some(UID),
            },
            None,
            None,
        )
        .unwrap();
    assert_eq!(all.len(), 2, "one total per generation, in the same month");
    assert_eq!(all[0].principal_generation, 0);
    assert_eq!(all[0].total_tokens, 100);
    assert_eq!(all[1].principal_generation, 1);
    assert_eq!(all[1].total_tokens, 7);

    // Bob, reporting on himself, sees only his own generation's total.
    let own = store.monthly(Scope::Own(UID), None, None).unwrap();
    assert_eq!(own.len(), 1);
    assert_eq!(own[0].total_tokens, 7);

    // The boundary itself must survive the rebuild, or the two would merge next
    // time anything was totalled.
    assert_eq!(store.retirements(Some(UID)).unwrap().len(), 1);
}

#[test]
fn accounts_are_not_mixed_by_the_rollup() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("usage.db");
    let old_at = NOW - 300 * DAY;
    {
        let mut store = Store::open(&path).unwrap();
        store
            .append(UID, Some("alice"), &at("a", old_at, 100), old_at + 1)
            .unwrap();
        store
            .append(OTHER, Some("carol"), &at("a", old_at, 250), old_at + 1)
            .unwrap();
    }

    retain::retain(&path, 180, NOW, None, false).unwrap();

    let store = Store::open(&path).unwrap();
    assert_eq!(
        store.monthly(Scope::Own(UID), None, None).unwrap()[0].total_tokens,
        100
    );
    assert_eq!(
        store.monthly(Scope::Own(OTHER), None, None).unwrap()[0].total_tokens,
        250
    );
}

#[test]
fn a_dry_run_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("usage.db");
    seeded(&path);
    let before = std::fs::metadata(&path).unwrap().len();

    let report = retain::retain(&path, 180, NOW, None, true).unwrap();
    assert_eq!(
        report.plan.events_rolled_up, 1,
        "it still says what it would do"
    );

    let store = Store::open(&path).unwrap();
    assert_eq!(
        store.query(Scope::Own(UID), None, None, 100).unwrap().len(),
        2,
        "both events are still there"
    );
    assert!(
        store
            .monthly(Scope::Own(UID), None, None)
            .unwrap()
            .is_empty()
    );
    assert!(store.detail_from().unwrap().is_none());
    assert_eq!(std::fs::metadata(&path).unwrap().len(), before);
    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|name| name.starts_with(".partial-retain"))
        .collect();
    assert!(leftovers.is_empty(), "no half-built database left behind");
}

#[test]
fn the_previous_database_is_kept() {
    // A mistaken --keep-days must be recoverable: the rolled-up detail exists in
    // exactly one place afterwards, and removing it is the operator's call.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("usage.db");
    seeded(&path);

    let report = retain::retain(&path, 180, NOW, None, false).unwrap();
    assert!(report.previous.exists(), "the pre-retention file is kept");

    let old = Store::open(&report.previous).unwrap();
    assert_eq!(
        old.query(Scope::Own(UID), None, None, 100).unwrap().len(),
        2,
        "the detail that was rolled up is still readable there"
    );
}

#[test]
fn nothing_happens_while_the_daemon_is_listening() {
    // The daemon is the only writer. Swapping the file under it would leave it
    // writing to an inode nothing reads.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("usage.db");
    seeded(&path);
    let socket = dir.path().join("usage.sock");
    let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();

    let error = retain::retain(&path, 180, NOW, Some(&socket), false)
        .expect_err("retention must refuse to run under a live daemon");
    assert!(matches!(error, RetainError::DaemonLive(_)));

    let store = Store::open(&path).unwrap();
    assert_eq!(
        store.query(Scope::Own(UID), None, None, 100).unwrap().len(),
        2
    );
}

#[test]
fn a_stale_socket_file_does_not_block_retention() {
    // A killed daemon leaves the socket file behind. Refusing on its mere
    // existence would make retention impossible to run after a crash.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("usage.db");
    seeded(&path);
    let socket = dir.path().join("usage.sock");
    {
        let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    }
    assert!(socket.exists(), "the file outlives the listener");

    retain::retain(&path, 180, NOW, Some(&socket), false)
        .expect("a socket nobody is listening on is not a running daemon");
}

#[test]
fn a_window_nothing_falls_outside_of_leaves_the_database_alone() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("usage.db");
    seeded(&path);

    let report = retain::retain(&path, 3_650, NOW, None, false).unwrap();
    assert_eq!(report.plan.events_rolled_up, 0);

    let store = Store::open(&path).unwrap();
    assert_eq!(
        store.query(Scope::Own(UID), None, None, 100).unwrap().len(),
        2
    );
    assert!(
        store
            .monthly(Scope::Own(UID), None, None)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn the_rebuilt_database_is_still_append_only() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("usage.db");
    seeded(&path);
    retain::retain(&path, 180, NOW, None, false).unwrap();

    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute("DELETE FROM usage_events", [])
        .expect_err("the rebuilt file keeps the append-only guard");
    conn.execute("UPDATE usage_events SET total_tokens = 1", [])
        .expect_err("and the no-update guard");
}

#[test]
fn unknown_usage_is_rolled_up_as_unknown_not_as_zero() {
    // Folding an unreported figure in as zero would make a total read as
    // complete when part of it is missing. The count survives the rollup.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("usage.db");
    let old_at = NOW - 300 * DAY;
    {
        let mut store = Store::open(&path).unwrap();
        store
            .append(UID, Some("alice"), &at("known", old_at, 100), old_at + 1)
            .unwrap();
        store
            .append(
                UID,
                Some("alice"),
                &event(json!({
                    "version": 1,
                    "client_event_id": "unknown",
                    "run_id": "unknown",
                    "attempt_id": "1",
                    "role": "worker",
                    "usage": {},
                    "usage_status": "unknown",
                    "occurred_at": old_at + 60
                })),
                old_at + 61,
            )
            .unwrap();
    }

    retain::retain(&path, 180, NOW, None, false).unwrap();

    let store = Store::open(&path).unwrap();
    let monthly = store.monthly(Scope::Own(UID), None, None).unwrap();
    assert_eq!(monthly[0].events, 2);
    assert_eq!(monthly[0].total_tokens, 100, "the unknown one adds nothing");
    assert_eq!(
        monthly[0].events_with_unknown_usage, 1,
        "and is still counted as unknown rather than as zero"
    );
}

#[test]
fn the_coverage_boundary_never_moves_backwards() {
    // Retention is not reversible. A second pass with a longer window must not
    // advertise coverage the first pass already removed, or a report would claim
    // completeness for a stretch whose detail is gone.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("usage.db");
    seeded(&path);

    let strict = retain::retain(&path, 150, NOW, None, false).unwrap();
    let boundary = strict.plan.detail_from;

    let lenient = retain::retain(&path, 3_650, NOW, None, false).unwrap();
    assert_eq!(
        lenient.plan.detail_from, boundary,
        "a longer window cannot restore what a shorter one rolled up"
    );

    let store = Store::open(&path).unwrap();
    assert_eq!(store.detail_from().unwrap(), Some(boundary));
    let wide = store
        .report(Scope::Own(UID), Some(NOW - 300 * DAY), Some(NOW))
        .unwrap();
    assert!(wide.detail_incomplete);
}

#[test]
fn the_rolled_up_count_accumulates_across_runs() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("usage.db");
    let old_at = NOW - 300 * DAY;
    {
        let mut store = Store::open(&path).unwrap();
        store
            .append(UID, Some("alice"), &at("a", old_at, 100), old_at + 1)
            .unwrap();
        store
            .append(
                UID,
                Some("alice"),
                &at("c", NOW - 170 * DAY, 400),
                NOW - 170 * DAY,
            )
            .unwrap();
    }

    retain::retain(&path, 180, NOW, None, false).unwrap();
    retain::retain(&path, 150, NOW, None, false).unwrap();

    let conn = rusqlite::Connection::open(&path).unwrap();
    let rolled: i64 = conn
        .query_row("SELECT rows_rolled FROM retention_state", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(rolled, 2, "both passes are counted, not just the last");
}
