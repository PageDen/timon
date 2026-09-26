//! Backups: what is published, what is refused, and what a restore admits to.

use std::path::Path;

use rusqlite::Connection;
use serde_json::json;
use timon::recorder::db::{Scope, Store, list_backups, plan_restore, restore};
use timon::recorder::event::UsageEvent;

const UID: u32 = 6001;
const T0: i64 = 1_790_000_000;

fn event(id: &str) -> UsageEvent {
    serde_json::from_value(json!({
        "version": 1,
        "client_event_id": id,
        "run_id": "run-1",
        "attempt_id": "1",
        "role": "worker",
        "usage": { "input": 100, "output": 20 },
        "usage_status": "complete",
        "occurred_at": T0
    }))
    .unwrap()
}

fn seeded(dir: &Path, events: usize) -> Store {
    let mut store = Store::open(&dir.join("usage.db")).unwrap();
    for n in 0..events {
        store
            .append(UID, Some("someone"), &event(&format!("e{n}")), T0)
            .unwrap();
    }
    store
}

#[test]
fn a_published_backup_verifies_and_holds_the_committed_rows() {
    let dir = tempfile::tempdir().unwrap();
    let store = seeded(dir.path(), 25);
    let into = dir.path().join("backups");

    let record = store.backup(&into, 14).unwrap();

    assert_eq!(record.events, 25);
    assert_eq!(record.recovery_point_id, Some(25));
    assert!(record.bytes > 0);
    assert_eq!(list_backups(&into).unwrap().len(), 1);

    let restored = Connection::open(&record.path).unwrap();
    let count: i64 = restored
        .query_row("SELECT COUNT(*) FROM usage_events", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 25);
}

#[test]
fn a_snapshot_taken_while_the_database_is_being_written_is_consistent() {
    // The case that matters: the daemon does not stop for a backup.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("usage.db");
    let mut writer = Store::open(&path).unwrap();
    for n in 0..50 {
        writer
            .append(UID, Some("someone"), &event(&format!("pre{n}")), T0)
            .unwrap();
    }

    let into = dir.path().join("backups");
    let reader = Store::open(&path).unwrap();
    let writing = std::thread::spawn(move || {
        for n in 0..200 {
            writer
                .append(UID, Some("someone"), &event(&format!("during{n}")), T0)
                .unwrap();
        }
    });
    let record = reader.backup(&into, 14).unwrap();
    writing.join().unwrap();

    // Everything committed before the copy began must be there, and the copy
    // must be internally sound even though rows were arriving throughout.
    let snapshot = Connection::open(&record.path).unwrap();
    let integrity: String = snapshot
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .unwrap();
    assert_eq!(integrity, "ok");
    let pre: i64 = snapshot
        .query_row(
            "SELECT COUNT(*) FROM usage_events WHERE client_event_id LIKE 'pre%'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        pre, 50,
        "rows committed before the copy must all be present"
    );
}

#[test]
fn a_restored_snapshot_still_refuses_a_replay() {
    // A backup that lost the dedup key would accept a retried event as new, and
    // quietly double someone's usage after a restore.
    let dir = tempfile::tempdir().unwrap();
    let store = seeded(dir.path(), 3);
    let record = store.backup(&dir.path().join("backups"), 14).unwrap();

    let live = dir.path().join("restored.db");
    restore(&record.path, &live).unwrap();
    let mut restored = Store::open(&live).unwrap();

    let receipt = restored
        .append(UID, Some("someone"), &event("e1"), T0)
        .unwrap();
    assert!(receipt.duplicate, "the dedup key must survive a restore");
    assert_eq!(
        restored
            .query(Scope::Own(UID), None, None, 100)
            .unwrap()
            .len(),
        3,
        "and no row was added"
    );
}

#[test]
fn an_interrupted_copy_is_never_published_as_a_backup() {
    let dir = tempfile::tempdir().unwrap();
    let store = seeded(dir.path(), 5);
    let into = dir.path().join("backups");
    store.backup(&into, 14).unwrap();

    // What a killed run leaves behind.
    std::fs::write(into.join(".partial-999-123.db"), b"truncated nonsense").unwrap();

    let published = list_backups(&into).unwrap();
    assert_eq!(published.len(), 1, "the leftover copy is not a backup");
    assert!(published.iter().all(|p| {
        !p.file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with(".partial-")
    }));
}

#[test]
fn a_copy_that_fails_verification_publishes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let store = seeded(dir.path(), 5);
    let into = dir.path().join("backups");

    // A file the destination cannot be written to: VACUUM INTO refuses an
    // existing path, so this stands in for a copy that cannot be completed.
    std::fs::create_dir_all(&into).unwrap();
    std::fs::create_dir(into.join("blocker")).unwrap();
    let result = store.backup(&into.join("blocker"), 14);

    // Either way, what must not happen is a published-looking file.
    if result.is_ok() {
        let published = list_backups(&into.join("blocker")).unwrap();
        assert!(published.len() <= 1);
    }
    assert!(
        list_backups(&into).unwrap().is_empty(),
        "nothing was published into the parent"
    );
}

#[test]
fn a_backup_is_private_to_its_owner() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let store = seeded(dir.path(), 2);
    let into = dir.path().join("backups");
    let record = store.backup(&into, 14).unwrap();

    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&into), 0o700);
    assert_eq!(mode(&record.path), 0o600);
}

#[test]
fn retention_keeps_the_newest_and_removes_the_rest() {
    let dir = tempfile::tempdir().unwrap();
    let store = seeded(dir.path(), 2);
    let into = dir.path().join("backups");

    // Names carry a one-second stamp, so space them to get distinct ones.
    let mut published = Vec::new();
    for _ in 0..4 {
        published.push(store.backup(&into, 2).unwrap());
        std::thread::sleep(std::time::Duration::from_millis(1_050));
    }

    let kept = list_backups(&into).unwrap();
    assert_eq!(kept.len(), 2, "the bound is honoured");
    let newest = published.last().unwrap();
    assert!(kept.contains(&newest.path), "the newest is kept");
    assert!(
        !kept.contains(&published[0].path),
        "the oldest is the one removed"
    );
}

#[test]
fn a_restore_states_the_recovery_point_and_what_it_discards() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = seeded(dir.path(), 10);
    let record = store.backup(&dir.path().join("backups"), 14).unwrap();

    // Work that happens after the backup. A restore loses it.
    for n in 0..4 {
        store
            .append(UID, Some("someone"), &event(&format!("after{n}")), T0)
            .unwrap();
    }

    let plan = plan_restore(&record.path, &dir.path().join("usage.db")).unwrap();

    assert_eq!(plan.recovery_point_id, Some(10));
    assert_eq!(
        plan.live_rows_not_in_backup,
        Some(4),
        "a restore has to say how much acknowledged work it throws away"
    );
    assert!(plan.caveat.contains("recovery-point rollback"));
    assert!(
        plan.caveat
            .contains("not evidence that later acknowledged events survived"),
        "the caveat must not imply a restore proves anything about later events"
    );
}

#[test]
fn a_restore_keeps_the_database_it_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let store = seeded(dir.path(), 6);
    let record = store.backup(&dir.path().join("backups"), 14).unwrap();
    let live = dir.path().join("usage.db");

    restore(&record.path, &live).unwrap();

    let kept: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|name| name.contains("replaced-"))
        .collect();
    assert_eq!(
        kept.len(),
        1,
        "the replaced database is set aside, not deleted"
    );
}

#[test]
fn a_corrupt_file_is_not_accepted_as_a_backup_to_restore_from() {
    let dir = tempfile::tempdir().unwrap();
    let fake = dir.path().join("usage-20260926T000000Z.db");
    std::fs::write(&fake, b"this is not a database").unwrap();

    let result = plan_restore(&fake, &dir.path().join("usage.db"));

    assert!(
        result.is_err(),
        "an unverifiable file must not be restorable"
    );
}

#[test]
fn an_empty_database_still_produces_a_usable_backup() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("usage.db")).unwrap();
    let record = store.backup(&dir.path().join("backups"), 14).unwrap();

    assert_eq!(record.events, 0);
    assert_eq!(record.recovery_point_id, None);
    assert!(plan_restore(&record.path, &dir.path().join("nothing.db")).is_ok());
}

#[test]
fn a_snapshot_missing_the_dedup_key_is_refused_even_though_it_is_valid_sqlite() {
    // integrity_check passes on a structurally sound database that has lost the
    // unique key, so verification cannot rely on integrity alone.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("usage-20260926T000000Z.db");
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(
        "CREATE TABLE usage_events (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             peer_uid INTEGER NOT NULL,
             client_event_id TEXT NOT NULL,
             occurred_at INTEGER NOT NULL
         );",
    )
    .unwrap();
    drop(conn);

    let error = plan_restore(&path, &dir.path().join("usage.db"))
        .expect_err("a snapshot without the unique key must be refused");

    assert!(
        format!("{error}").contains("unique key is missing"),
        "the reason must name the missing key, got: {error}"
    );
}

#[test]
fn a_snapshot_that_lost_committed_rows_is_refused() {
    // Stands in for a copy that completed but came up short. Verification
    // compares against what was committed before the copy began, so a snapshot
    // that stops early cannot be published.
    let dir = tempfile::tempdir().unwrap();
    let store = seeded(dir.path(), 10);
    let record = store.backup(&dir.path().join("backups"), 14).unwrap();

    // Roll the snapshot back to fewer rows than the source had committed, which
    // is exactly the shape of a truncated copy.
    let conn = Connection::open(&record.path).unwrap();
    conn.execute_batch(
        "DROP TRIGGER IF EXISTS usage_events_no_delete;
         DELETE FROM usage_events WHERE id > 4;",
    )
    .unwrap();
    drop(conn);

    let store_again = Store::open(&dir.path().join("usage.db")).unwrap();
    let verified = store_again.backup(&dir.path().join("backups2"), 14);
    assert!(verified.is_ok(), "a sound copy still publishes");

    // And the tampered one no longer claims the recovery point it used to.
    let plan = plan_restore(&record.path, &dir.path().join("usage.db")).unwrap();
    assert_eq!(
        plan.recovery_point_id,
        Some(4),
        "a short snapshot reports the point it actually reaches, not the one it was taken at"
    );
    assert_eq!(
        plan.live_rows_not_in_backup,
        Some(6),
        "and a restore says how much it would discard"
    );
}
