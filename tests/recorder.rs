//! Recorder contracts that do not need a second account.
//!
//! The cross-account half of amendment A1 — that one principal cannot reach or
//! alter another's rows through the running service — is exercised by
//! `tests/cross-user.sh`, which uses real separate UIDs. Nothing here proves
//! isolation on its own.

use rusqlite::Connection;
use serde_json::json;
use timon::recorder::db::{AppendError, Scope, Store, totals};
use timon::recorder::event::UsageEvent;

const UID_A: u32 = 4001;
const UID_B: u32 = 4002;

fn store(dir: &std::path::Path) -> Store {
    Store::open(&dir.join("usage.db")).unwrap()
}

/// Events are built from JSON so the tests exercise the wire format producers
/// actually send, not a struct literal only Rust can express.
fn event(value: serde_json::Value) -> UsageEvent {
    serde_json::from_value(value).expect("event should parse")
}

fn base() -> serde_json::Value {
    json!({
        "version": 1,
        "client_event_id": "worker:run-1:1",
        "run_id": "run-1",
        "attempt_id": "1",
        "role": "worker",
        "provider": "openai",
        "model": "gpt-5.5",
        "usage": { "input": 100, "cached_input": 40, "output": 10, "reasoning_output": 2 },
        "usage_status": "complete",
        "duration_ms": 1234,
        "occurred_at": 1_790_000_000
    })
}

#[test]
fn a_replayed_delivery_returns_the_committed_row_instead_of_adding_another() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = store(dir.path());
    let event = event(base());

    let first = store
        .append(UID_A, Some("a"), &event, 1_790_000_001)
        .unwrap();
    let replay = store
        .append(UID_A, Some("a"), &event, 1_790_000_999)
        .unwrap();

    assert!(!first.duplicate);
    assert!(replay.duplicate, "a retry must not be stored twice");
    assert_eq!(first.id, replay.id);
    let rows = store.query(Scope::Own(UID_A), None, None, 100).unwrap();
    assert_eq!(rows.len(), 1, "one event, one row");
}

#[test]
fn the_same_event_id_with_different_content_is_refused_not_overwritten() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = store(dir.path());
    store
        .append(UID_A, Some("a"), &event(base()), 1_790_000_001)
        .unwrap();

    let mut altered = base();
    altered["usage"]["output"] = json!(999);
    let error = store
        .append(UID_A, Some("a"), &event(altered), 1_790_000_002)
        .expect_err("a different payload under a stored id must be refused");

    assert!(matches!(error, AppendError::Conflict { .. }));
    let rows = store.query(Scope::Own(UID_A), None, None, 100).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].output.value(),
        Some(10),
        "the stored row is unchanged"
    );
}

#[test]
fn the_same_event_id_from_two_principals_stores_both() {
    // Deduplication is scoped to the principal. A shared id must not let one
    // account suppress another account's record.
    let dir = tempfile::tempdir().unwrap();
    let mut store = store(dir.path());
    let event = event(base());

    store
        .append(UID_A, Some("a"), &event, 1_790_000_001)
        .unwrap();
    let second = store
        .append(UID_B, Some("b"), &event, 1_790_000_001)
        .unwrap();

    assert!(!second.duplicate);
    assert_eq!(
        store
            .query(Scope::Own(UID_A), None, None, 100)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        store
            .query(Scope::Own(UID_B), None, None, 100)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn an_unreported_count_is_stored_as_null_and_reads_back_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = store(dir.path());
    let mut value = base();
    value["usage"] = json!({ "input": null, "output": null });
    value["usage_status"] = json!("unknown");
    store
        .append(UID_A, Some("a"), &event(value), 1_790_000_001)
        .unwrap();

    let rows = store.query(Scope::Own(UID_A), None, None, 100).unwrap();
    assert_eq!(rows[0].input.value(), None, "unknown must not become zero");
    assert_eq!(rows[0].total.value(), None);

    let raw = Connection::open(dir.path().join("usage.db")).unwrap();
    let stored: Option<i64> = raw
        .query_row("SELECT input_tokens FROM usage_events", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(stored, None, "the column itself must be NULL, not 0");
}

#[test]
fn updates_and_deletes_are_refused_by_the_database() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = store(dir.path());
    store
        .append(UID_A, Some("a"), &event(base()), 1_790_000_001)
        .unwrap();

    let raw = Connection::open(dir.path().join("usage.db")).unwrap();
    let update = raw.execute("UPDATE usage_events SET output_tokens = 1", []);
    let delete = raw.execute("DELETE FROM usage_events", []);

    assert!(update.is_err(), "usage_events must be append-only");
    assert!(delete.is_err(), "usage_events must be append-only");
    assert_eq!(
        store
            .query(Scope::Own(UID_A), None, None, 100)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn a_correction_may_only_target_your_own_row() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = store(dir.path());
    let mine = store
        .append(UID_A, Some("a"), &event(base()), 1_790_000_001)
        .unwrap();

    let mut theirs = base();
    theirs["client_event_id"] = json!("worker:run-1:1-correction");
    theirs["corrects"] = json!(mine.id);
    let error = store
        .append(UID_B, Some("b"), &event(theirs.clone()), 1_790_000_002)
        .expect_err("correcting another principal's row must be refused");
    assert!(matches!(error, AppendError::UncorrectableTarget { .. }));

    // The owner may correct it, and the correction is a new row, not an edit.
    let correction = store
        .append(UID_A, Some("a"), &event(theirs), 1_790_000_002)
        .unwrap();
    assert_ne!(correction.id, mine.id);
    let rows = store.query(Scope::Own(UID_A), None, None, 100).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].corrects, Some(mine.id));
}

#[test]
fn identity_claims_in_the_payload_do_not_affect_the_stored_event() {
    // A client may send uid/user/admin. They are ignored, and because they are
    // outside the canonical payload they cannot make a replay look different.
    let plain = event(base());
    let mut claiming = base();
    claiming["uid"] = json!(0);
    claiming["user"] = json!("root");
    claiming["admin"] = json!(true);
    let claiming = event(claiming);

    assert!(claiming.carried_identity_claim());
    assert!(!plain.carried_identity_claim());
    assert_eq!(
        plain.canonical_payload(),
        claiming.canonical_payload(),
        "an ignored claim must not change the event's identity"
    );

    let dir = tempfile::tempdir().unwrap();
    let mut store = store(dir.path());
    store
        .append(UID_A, Some("a"), &claiming, 1_790_000_001)
        .unwrap();
    let rows = store.query(Scope::Own(UID_A), None, None, 100).unwrap();
    assert_eq!(rows[0].peer_uid, UID_A, "the connection decides the owner");
    assert_eq!(rows[0].peer_username.as_deref(), Some("a"));
}

#[test]
fn an_unknown_field_is_refused_but_an_ignored_identity_claim_is_not() {
    let mut typo = base();
    typo["input_tokens"] = json!(5); // a plausible misspelling of a real field
    assert!(
        serde_json::from_value::<UsageEvent>(typo).is_err(),
        "an unrecognised field is far more likely a mistake than a claim"
    );
}

#[test]
fn a_subset_larger_than_its_whole_is_refused() {
    let mut value = base();
    value["usage"]["cached_input"] = json!(1_000);
    assert!(event(value).validate().is_err());

    let mut value = base();
    value["usage"]["reasoning_output"] = json!(1_000);
    assert!(event(value).validate().is_err());
}

#[test]
fn an_unsupported_version_is_refused() {
    let mut value = base();
    value["version"] = json!(99);
    assert!(event(value).validate().is_err());
}

#[test]
fn a_negative_or_fractional_count_is_refused_rather_than_called_unknown() {
    for bad in [json!(-1), json!(1.5), json!("12")] {
        let mut value = base();
        value["usage"]["input"] = bad.clone();
        assert!(
            serde_json::from_value::<UsageEvent>(value).is_err(),
            "{bad} must not be accepted"
        );
    }
}

#[test]
fn own_scope_never_returns_another_principals_rows() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = store(dir.path());
    store
        .append(UID_A, Some("a"), &event(base()), 1_790_000_001)
        .unwrap();
    let mut other = base();
    other["run_id"] = json!("run-2");
    store
        .append(UID_B, Some("b"), &event(other), 1_790_000_001)
        .unwrap();

    let mine = store.query(Scope::Own(UID_A), None, None, 100).unwrap();
    assert_eq!(mine.len(), 1);
    assert!(mine.iter().all(|row| row.peer_uid == UID_A));

    let every = store
        .query(Scope::Admin { only_uid: None }, None, None, 100)
        .unwrap();
    assert_eq!(every.len(), 2, "an administrator sees both");
}

#[test]
fn totals_exclude_unknown_usage_and_count_it_separately() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = store(dir.path());
    store
        .append(UID_A, Some("a"), &event(base()), 1_790_000_001)
        .unwrap();
    let mut unknown = base();
    unknown["client_event_id"] = json!("worker:run-1:2");
    unknown["usage"] = json!({});
    unknown["usage_status"] = json!("unknown");
    store
        .append(UID_A, Some("a"), &event(unknown), 1_790_000_001)
        .unwrap();

    let rows = store.query(Scope::Own(UID_A), None, None, 100).unwrap();
    let totals = totals(&rows);

    assert_eq!(totals.events, 2);
    assert_eq!(
        totals.total_tokens, 110,
        "100 input + 10 output, counted once"
    );
    assert_eq!(
        totals.events_with_unknown_usage, 1,
        "a missing total must be visible, not absorbed into the sum"
    );
}

#[test]
fn a_delayed_delivery_is_marked_late_and_keeps_its_original_timestamp() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = store(dir.path());
    let occurred = 1_790_000_000;
    store
        .append(UID_A, Some("a"), &event(base()), occurred + 4_000)
        .unwrap();

    let rows = store.query(Scope::Own(UID_A), None, None, 100).unwrap();
    assert!(rows[0].late, "a replay long after the fact is late");
    assert_eq!(
        rows[0].occurred_at, occurred,
        "the producer's clock is preserved"
    );
    assert_eq!(rows[0].received_at, occurred + 4_000);
}

#[test]
fn the_database_is_opened_with_the_durability_the_contract_assumes() {
    let dir = tempfile::tempdir().unwrap();
    let _store = store(dir.path());
    let raw = Connection::open(dir.path().join("usage.db")).unwrap();

    let journal: String = raw
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .unwrap();
    let synchronous: i64 = raw
        .query_row("PRAGMA synchronous", [], |row| row.get(0))
        .unwrap();

    assert_eq!(journal.to_lowercase(), "wal");
    // 2 is FULL. Anything lower would let an acknowledged event vanish in a
    // power loss, which the at-least-once contract does not allow.
    assert_eq!(synchronous, 2);
}

#[test]
fn a_time_window_selects_by_when_the_work_happened_not_when_it_arrived() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = store(dir.path());
    for (id, occurred) in [("a", 1_000_i64), ("b", 2_000), ("c", 3_000)] {
        let mut value = base();
        value["client_event_id"] = json!(id);
        value["occurred_at"] = json!(occurred);
        store
            .append(UID_A, Some("a"), &event(value), 9_999)
            .unwrap();
    }

    let window = store
        .query(Scope::Own(UID_A), Some(1_500), Some(2_500), 100)
        .unwrap();
    assert_eq!(window.len(), 1);
    assert_eq!(window[0].occurred_at, 2_000);
}

#[test]
fn every_response_survives_the_round_trip_it_is_sent_through() {
    // A receipt omits `identity_claim_ignored` when it is false, so the field
    // has to default on the way back in. Without that, an ordinary receipt is
    // undecodable by the client and a stored event looks like a failure.
    for wire in [
        r#"{"type":"receipt","id":1,"duplicate":false}"#,
        r#"{"type":"receipt","id":2,"duplicate":true,"identity_claim_ignored":true}"#,
        r#"{"type":"rows","rows":[],"totals":{"events":0,"total_tokens":0,"events_with_unknown_usage":0,"late_events":0},"scope_uid":7}"#,
        r#"{"type":"error","code":"conflict","message":"x"}"#,
    ] {
        let decoded: timon::recorder::protocol::Response =
            serde_json::from_str(wire).unwrap_or_else(|e| panic!("{wire} failed to decode: {e}"));
        let reencoded = serde_json::to_string(&decoded).unwrap();
        serde_json::from_str::<timon::recorder::protocol::Response>(&reencoded)
            .unwrap_or_else(|e| panic!("{reencoded} failed to decode again: {e}"));
    }
}

#[test]
fn every_request_survives_the_round_trip_it_is_sent_through() {
    for wire in [
        r#"{"type":"query"}"#,
        r#"{"type":"query","since":1,"until":2,"only_uid":3,"limit":4}"#,
    ] {
        let decoded: timon::recorder::protocol::Request =
            serde_json::from_str(wire).unwrap_or_else(|e| panic!("{wire} failed to decode: {e}"));
        let reencoded = serde_json::to_string(&decoded).unwrap();
        serde_json::from_str::<timon::recorder::protocol::Request>(&reencoded)
            .unwrap_or_else(|e| panic!("{reencoded} failed to decode again: {e}"));
    }
}
