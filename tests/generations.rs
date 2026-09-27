//! The uid-reuse boundary.
//!
//! `SO_PEERCRED` reports a uid and nothing else, and Linux hands a uid out again
//! after an account is removed. Without a recorded boundary the next holder of
//! uid 1001 inherits the previous holder's history — both in what a report shows
//! them and in the dedup key, because `client_event_id` is derived from role, run
//! id and attempt id rather than generated, so two people easily produce the same
//! one.

use serde_json::json;
use timon::recorder::db::{AppendError, Scope, Store};
use timon::recorder::event::UsageEvent;

const UID: u32 = 1001;

fn store(dir: &std::path::Path) -> Store {
    Store::open(&dir.join("usage.db")).unwrap()
}

fn event(value: serde_json::Value) -> UsageEvent {
    serde_json::from_value(value).expect("event should parse")
}

fn base(event_id: &str, total_in: u64) -> serde_json::Value {
    json!({
        "version": 1,
        "client_event_id": event_id,
        "run_id": "run-1",
        "attempt_id": "1",
        "role": "worker",
        "provider": "openai",
        "model": "gpt-5.5",
        "usage": { "input": total_in, "cached_input": 0, "output": 0, "reasoning_output": 0 },
        "usage_status": "complete",
        "duration_ms": 10,
        "occurred_at": 1_790_000_000
    })
}

#[test]
fn a_recycled_uid_reusing_an_event_id_is_a_new_row_not_a_duplicate() {
    // The failure this prevents: alice runs `--run-id run-1`, her account is
    // removed, bob inherits uid 1001 and runs `--run-id run-1`. On the old key
    // bob's event collides with alice's — silently dropped as a duplicate when
    // the content matches, refused as a conflict when it does not.
    let dir = tempfile::tempdir().unwrap();
    let mut store = store(dir.path());

    let alice = store
        .append(
            UID,
            Some("alice"),
            &event(base("worker:run-1:1", 100)),
            1_790_000_001,
        )
        .unwrap();
    assert!(!alice.duplicate);

    store
        .retire_principal(UID, Some("alice"), Some("account removed"), 1_790_000_500)
        .unwrap();

    let bob = store
        .append(
            UID,
            Some("bob"),
            &event(base("worker:run-1:1", 100)),
            1_790_001_000,
        )
        .unwrap();

    assert!(
        !bob.duplicate,
        "bob's event shares alice's id but belongs to a later generation"
    );
    assert_ne!(alice.id, bob.id, "two events, two rows");
}

#[test]
fn a_recycled_uid_with_different_content_is_not_refused_as_a_conflict() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = store(dir.path());
    store
        .append(
            UID,
            Some("alice"),
            &event(base("worker:run-1:1", 100)),
            1_790_000_001,
        )
        .unwrap();
    store
        .retire_principal(UID, Some("alice"), None, 1_790_000_500)
        .unwrap();

    // Same id, different numbers. Before the boundary existed this was a
    // Conflict, which lost bob's event entirely.
    store
        .append(
            UID,
            Some("bob"),
            &event(base("worker:run-1:1", 777)),
            1_790_001_000,
        )
        .expect("a later generation is not in conflict with an earlier one");
}

#[test]
fn a_new_holder_of_a_uid_cannot_see_the_previous_holders_usage() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = store(dir.path());
    store
        .append(
            UID,
            Some("alice"),
            &event(base("worker:run-1:1", 100)),
            1_790_000_001,
        )
        .unwrap();
    store
        .retire_principal(UID, Some("alice"), None, 1_790_000_500)
        .unwrap();
    store
        .append(
            UID,
            Some("bob"),
            &event(base("worker:run-9:1", 5)),
            1_790_001_000,
        )
        .unwrap();

    let own = store.query(Scope::Own(UID), None, None, 100).unwrap();
    assert_eq!(own.len(), 1, "bob sees his own event and not alice's");
    assert_eq!(own[0].client_event_id, "worker:run-9:1");
    assert_eq!(own[0].total.value(), Some(5));

    let report = store.report(Scope::Own(UID), None, None).unwrap();
    assert_eq!(report.principals.len(), 1);
    assert_eq!(report.principals[0].principal_generation, 1);
    assert_eq!(
        report.principals[0].total_tokens, 5,
        "alice's 100 tokens are not bob's"
    );
}

#[test]
fn an_administrator_sees_both_generations_and_they_are_not_summed() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = store(dir.path());
    store
        .append(
            UID,
            Some("alice"),
            &event(base("worker:run-1:1", 100)),
            1_790_000_001,
        )
        .unwrap();
    store
        .retire_principal(UID, Some("alice"), None, 1_790_000_500)
        .unwrap();
    store
        .append(
            UID,
            Some("bob"),
            &event(base("worker:run-9:1", 5)),
            1_790_001_000,
        )
        .unwrap();

    let report = store
        .report(
            Scope::Admin {
                only_uid: Some(UID),
            },
            None,
            None,
        )
        .unwrap();

    assert_eq!(
        report.principals.len(),
        2,
        "one row per generation, so the two are never added together"
    );
    let gens: Vec<u32> = report
        .principals
        .iter()
        .map(|p| p.principal_generation)
        .collect();
    assert_eq!(gens, vec![0, 1]);
    assert_eq!(report.principals[0].total_tokens, 100);
    assert_eq!(report.principals[0].username.as_deref(), Some("alice"));
    assert_eq!(report.principals[1].total_tokens, 5);
    assert_eq!(report.principals[1].username.as_deref(), Some("bob"));
}

#[test]
fn inheriting_a_uid_does_not_carry_the_right_to_correct_its_history() {
    // Correcting across principals is already refused. A recycled uid is the same
    // problem wearing the same number.
    let dir = tempfile::tempdir().unwrap();
    let mut store = store(dir.path());
    let alice = store
        .append(
            UID,
            Some("alice"),
            &event(base("worker:run-1:1", 100)),
            1_790_000_001,
        )
        .unwrap();
    store
        .retire_principal(UID, Some("alice"), None, 1_790_000_500)
        .unwrap();

    let mut correction = base("worker:run-1:1-fix", 1);
    correction["corrects"] = json!(alice.id);
    let error = store
        .append(UID, Some("bob"), &event(correction), 1_790_001_000)
        .expect_err("bob must not rewrite alice's figures");

    assert!(matches!(error, AppendError::UncorrectableTarget { .. }));
}

#[test]
fn a_retirement_is_recorded_once_and_is_visible_afterwards() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = store(dir.path());

    assert_eq!(store.current_generation(UID).unwrap(), 0);
    let first = store
        .retire_principal(UID, Some("alice"), Some("left the team"), 1_790_000_500)
        .unwrap();
    assert_eq!(first.generation, 0, "the boundary closes generation 0");
    assert_eq!(store.current_generation(UID).unwrap(), 1);

    store
        .retire_principal(UID, Some("alice"), None, 1_790_000_500)
        .expect_err("the same boundary twice would invent a generation nobody used");

    let recorded = store.retirements(Some(UID)).unwrap();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].retired_username.as_deref(), Some("alice"));
    assert_eq!(recorded[0].note.as_deref(), Some("left the team"));
}

#[test]
fn a_v1_database_migrates_with_every_row_intact_and_no_boundaries() {
    // The shape schema v1 shipped, including the old dedup key. Anything already
    // recorded predates the column, so it is all generation 0: there was no
    // boundary to place it after.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("usage.db");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE schema_version (version INTEGER NOT NULL);
            INSERT INTO schema_version (version) VALUES (1);
            CREATE TABLE usage_events (
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
            CREATE TRIGGER usage_events_no_update
            BEFORE UPDATE ON usage_events BEGIN
                SELECT RAISE(ABORT, 'usage_events is append-only');
            END;
            CREATE TRIGGER usage_events_no_delete
            BEFORE DELETE ON usage_events BEGIN
                SELECT RAISE(ABORT, 'usage_events is append-only');
            END;
            INSERT INTO usage_events (
                peer_uid, peer_username, client_event_id, run_id, attempt_id, role,
                total_tokens, usage_status, occurred_at, received_at, late, client_payload
            ) VALUES
              (1001, 'alice', 'worker:run-1:1', 'run-1', '1', 'worker', 100, 'complete',
               1790000000, 1790000001, 0, '{}'),
              (1002, 'carol', 'worker:run-2:1', 'run-2', '1', 'worker', 250, 'complete',
               1790000010, 1790000011, 0, '{}');
            "#,
        )
        .unwrap();
        // A correction, to prove ids survive the rebuild and keep resolving.
        conn.execute_batch(
            r#"
            INSERT INTO usage_events (
                peer_uid, peer_username, client_event_id, run_id, attempt_id, role,
                total_tokens, usage_status, occurred_at, received_at, late, corrects,
                client_payload
            ) VALUES
              (1002, 'carol', 'worker:run-2:1-fix', 'run-2', '1', 'worker', 300, 'complete',
               1790000020, 1790000021, 0, 2, '{}');
            "#,
        )
        .unwrap();
    }

    let store = Store::open(&path).unwrap();

    let version: i64 = {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.query_row("SELECT version FROM schema_version", [], |row| row.get(0))
            .unwrap()
    };
    assert_eq!(version, 2);

    let all = store
        .report(Scope::Admin { only_uid: None }, None, None)
        .unwrap();
    assert_eq!(all.principals.len(), 2, "both accounts survive");
    assert!(
        all.principals.iter().all(|p| p.principal_generation == 0),
        "pre-existing rows have no boundary before them"
    );
    // 100 for alice; carol's 250 is superseded by the 300 correction.
    assert_eq!(all.principals[0].total_tokens, 100);
    assert_eq!(all.principals[1].total_tokens, 300);
    assert_eq!(
        all.superseded_by_corrections, 1,
        "the correction still resolves"
    );
}

#[test]
fn appending_still_works_after_the_migration() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("usage.db");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE schema_version (version INTEGER NOT NULL);
            INSERT INTO schema_version (version) VALUES (1);
            CREATE TABLE usage_events (
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
            "#,
        )
        .unwrap();
    }

    let mut store = Store::open(&path).unwrap();
    store
        .append(
            UID,
            Some("alice"),
            &event(base("worker:run-1:1", 100)),
            1_790_000_001,
        )
        .unwrap();
    let rows = store.query(Scope::Own(UID), None, None, 10).unwrap();
    assert_eq!(rows.len(), 1);
}

#[test]
fn an_earlier_generation_is_labelled_rather_than_looking_like_a_duplicate_row() {
    // Two unlabelled groups under one uid read as the same account listed twice.
    // An administrator looking at a recycled uid has to be able to tell which
    // figures belong to the current holder.
    let dir = tempfile::tempdir().unwrap();
    let mut store = store(dir.path());
    store
        .append(
            UID,
            Some("alice"),
            &event(base("worker:run-1:1", 100)),
            1_790_000_001,
        )
        .unwrap();
    store
        .retire_principal(UID, Some("alice"), None, 1_790_000_500)
        .unwrap();
    store
        .append(
            UID,
            Some("bob"),
            &event(base("worker:run-9:1", 5)),
            1_790_001_000,
        )
        .unwrap();

    let report = store
        .report(
            Scope::Admin {
                only_uid: Some(UID),
            },
            None,
            None,
        )
        .unwrap();
    let text = timon::recorder::render::text(&report);

    assert!(text.contains("generation 0"), "got: {text}");
    assert!(text.contains("generation 1"), "got: {text}");
    assert!(
        text.contains("an earlier holder of this uid"),
        "the closed generation says whose figures these are not, got: {text}"
    );
}

#[test]
fn a_single_generation_is_not_cluttered_with_a_label() {
    // The ordinary case is one generation, and it should read exactly as before.
    let dir = tempfile::tempdir().unwrap();
    let mut store = store(dir.path());
    store
        .append(
            UID,
            Some("alice"),
            &event(base("worker:run-1:1", 100)),
            1_790_000_001,
        )
        .unwrap();

    let report = store.report(Scope::Own(UID), None, None).unwrap();
    let text = timon::recorder::render::text(&report);
    assert!(text.contains("alice (uid 1001)"), "got: {text}");
    assert!(!text.contains("generation"), "got: {text}");
}
