//! Reports: what the totals mean, and the caveats that must travel with them.

use serde_json::json;
use timon::recorder::db::{Scope, Store};
use timon::recorder::event::UsageEvent;
use timon::recorder::render;

const UID_A: u32 = 5001;
const UID_B: u32 = 5002;
const T0: i64 = 1_790_000_000;

fn store(dir: &std::path::Path) -> Store {
    Store::open(&dir.join("usage.db")).unwrap()
}

fn event(value: serde_json::Value) -> UsageEvent {
    serde_json::from_value(value).expect("event should parse")
}

fn base(id: &str) -> serde_json::Value {
    json!({
        "version": 1,
        "client_event_id": id,
        "run_id": "run-1",
        "attempt_id": "1",
        "role": "worker",
        "usage": { "input": 100, "cached_input": 10, "output": 20, "reasoning_output": 5 },
        "usage_status": "complete",
        "occurred_at": T0
    })
}

#[test]
fn a_correction_is_counted_once_in_place_of_what_it_corrects() {
    // The risk is charging twice: once for the original and once for the fix.
    let dir = tempfile::tempdir().unwrap();
    let mut store = store(dir.path());
    let original = store
        .append(UID_A, Some("a"), &event(base("e1")), T0)
        .unwrap();

    let mut correction = base("e1-fix");
    correction["usage"]["output"] = json!(60);
    correction["corrects"] = json!(original.id);
    store
        .append(UID_A, Some("a"), &event(correction), T0)
        .unwrap();

    let report = store.report(Scope::Own(UID_A), None, None).unwrap();
    let totals = &report.principals[0];

    assert_eq!(
        totals.events, 1,
        "the corrected row is replaced, not added to"
    );
    assert_eq!(totals.total_tokens, 160, "100 input + 60 corrected output");
    assert_eq!(totals.corrections_applied, 1);
    assert_eq!(report.superseded_by_corrections, 1);
}

#[test]
fn a_chain_of_corrections_counts_only_the_last() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = store(dir.path());
    let first = store
        .append(UID_A, Some("a"), &event(base("e1")), T0)
        .unwrap();

    let mut second = base("e2");
    second["corrects"] = json!(first.id);
    second["usage"]["output"] = json!(40);
    let second = store.append(UID_A, Some("a"), &event(second), T0).unwrap();

    let mut third = base("e3");
    third["corrects"] = json!(second.id);
    third["usage"]["output"] = json!(70);
    store.append(UID_A, Some("a"), &event(third), T0).unwrap();

    let report = store.report(Scope::Own(UID_A), None, None).unwrap();

    assert_eq!(report.principals[0].events, 1);
    assert_eq!(
        report.principals[0].total_tokens, 170,
        "only the newest survives"
    );
    assert_eq!(report.superseded_by_corrections, 2);
}

#[test]
fn an_event_with_unknown_usage_is_counted_separately_rather_than_as_zero() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = store(dir.path());
    store
        .append(UID_A, Some("a"), &event(base("e1")), T0)
        .unwrap();
    let mut unknown = base("e2");
    unknown["usage"] = json!({});
    unknown["usage_status"] = json!("unknown");
    store.append(UID_A, Some("a"), &event(unknown), T0).unwrap();

    let totals = &store
        .report(Scope::Own(UID_A), None, None)
        .unwrap()
        .principals[0];

    assert_eq!(totals.events, 2);
    assert_eq!(
        totals.total_tokens, 120,
        "the unknown event contributes nothing"
    );
    assert_eq!(
        totals.events_with_unknown_usage, 1,
        "and its absence has to be visible next to the total"
    );
}

#[test]
fn a_partial_event_is_reported_as_a_lower_bound() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = store(dir.path());
    let mut partial = base("e1");
    partial["usage_status"] = json!("partial");
    store.append(UID_A, Some("a"), &event(partial), T0).unwrap();

    let totals = &store
        .report(Scope::Own(UID_A), None, None)
        .unwrap()
        .principals[0];
    assert_eq!(totals.events_with_partial_usage, 1);
    assert!(
        render::text(&store.report(Scope::Own(UID_A), None, None).unwrap()).contains("lower bound")
    );
}

#[test]
fn a_late_event_is_flagged_so_an_earlier_report_can_be_distrusted() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = store(dir.path());
    store
        .append(UID_A, Some("a"), &event(base("e1")), T0 + 10_000)
        .unwrap();

    let totals = &store
        .report(Scope::Own(UID_A), None, None)
        .unwrap()
        .principals[0];
    assert_eq!(totals.late_events, 1);
    assert!(
        render::text(&store.report(Scope::Own(UID_A), None, None).unwrap())
            .contains("arrived late"),
        "a reader has to be told the window may have changed"
    );
}

#[test]
fn own_scope_totals_only_the_callers_usage() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = store(dir.path());
    store
        .append(UID_A, Some("a"), &event(base("e1")), T0)
        .unwrap();
    store
        .append(UID_B, Some("b"), &event(base("e1")), T0)
        .unwrap();

    let mine = store.report(Scope::Own(UID_A), None, None).unwrap();
    assert_eq!(mine.principals.len(), 1);
    assert_eq!(mine.principals[0].peer_uid, UID_A);

    let all = store
        .report(Scope::Admin { only_uid: None }, None, None)
        .unwrap();
    assert_eq!(all.principals.len(), 2, "an administrator sees both");
}

#[test]
fn a_uid_seen_under_two_names_is_flagged() {
    // A rename is harmless. A uid recycled *without* a recorded retirement is
    // not: it merges two people's history, and this is the only signal that it
    // happened. Recording the retirement is what separates them; see
    // `tests/generations.rs`.
    let dir = tempfile::tempdir().unwrap();
    let mut store = store(dir.path());
    store
        .append(UID_A, Some("oldname"), &event(base("e1")), T0)
        .unwrap();
    store
        .append(UID_A, Some("newname"), &event(base("e2")), T0)
        .unwrap();

    let report = store.report(Scope::Own(UID_A), None, None).unwrap();
    assert_eq!(report.principals[0].names_seen, 2);
    assert_eq!(
        report.principals[0].username.as_deref(),
        Some("newname"),
        "the latest name is shown for display"
    );
    let text = render::text(&report);
    assert!(
        text.contains("2 different names seen within this generation"),
        "the rename is surfaced, got: {text}"
    );
    assert!(
        text.contains("without recording a retirement"),
        "and so is the case where it is not a rename, got: {text}"
    );
}

#[test]
fn a_window_excludes_work_that_happened_outside_it() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = store(dir.path());
    for (id, at) in [
        ("early", T0 - 10_000),
        ("inside", T0),
        ("late", T0 + 10_000),
    ] {
        let mut value = base(id);
        value["occurred_at"] = json!(at);
        store.append(UID_A, Some("a"), &event(value), T0).unwrap();
    }

    let report = store
        .report(Scope::Own(UID_A), Some(T0 - 100), Some(T0 + 100))
        .unwrap();
    assert_eq!(report.principals[0].events, 1);
}

#[test]
fn totals_are_not_truncated_by_the_row_limit_a_query_would_apply() {
    // Aggregating in SQL rather than over fetched rows is what keeps a large
    // window from silently understating someone's usage.
    let dir = tempfile::tempdir().unwrap();
    let mut store = store(dir.path());
    for n in 0..1_500 {
        store
            .append(UID_A, Some("a"), &event(base(&format!("e{n}"))), T0)
            .unwrap();
    }

    let rows = store.query(Scope::Own(UID_A), None, None, 100).unwrap();
    let report = store.report(Scope::Own(UID_A), None, None).unwrap();

    assert_eq!(rows.len(), 100, "a query is limited, as asked");
    assert_eq!(report.principals[0].events, 1_500, "a report is not");
    assert_eq!(report.principals[0].total_tokens, 1_500 * 120);
}

#[test]
fn an_empty_window_says_so_instead_of_printing_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    let report = store.report(Scope::Own(UID_A), None, None).unwrap();
    assert!(render::text(&report).contains("no usage recorded"));
}

#[test]
fn every_rendering_carries_the_visibility_and_shared_quota_caveats() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = store(dir.path());
    store
        .append(UID_A, Some("a"), &event(base("e1")), T0)
        .unwrap();
    let report = store.report(Scope::Own(UID_A), None, None).unwrap();

    for rendering in [render::text(&report), render::csv(&report)] {
        assert!(
            rendering.contains("not billing"),
            "a total without this reads as a bill"
        );
        assert!(rendering.contains("shared provider quota"));
    }
    // Also on the wire, so a consumer cannot drop them by accident.
    let wire = serde_json::to_string(&report).unwrap();
    assert!(wire.contains("not billing") && wire.contains("shared provider quota"));
}

#[test]
fn csv_quotes_anything_that_would_change_the_shape_of_a_row() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = store(dir.path());
    store
        .append(
            UID_A,
            Some("odd,\"name\"\nwith breaks"),
            &event(base("e1")),
            T0,
        )
        .unwrap();
    let report = store.report(Scope::Own(UID_A), None, None).unwrap();

    let csv = render::csv(&report);
    let data = csv.lines().find(|l| l.starts_with("5001")).unwrap();
    assert!(
        data.contains("\"odd,\"\"name\"\"\nwith breaks\"") || data.contains("\"odd,\"\"name\"\""),
        "embedded commas, quotes and newlines must be escaped, got: {data}"
    );
    // The header row must still have the column count it declares.
    let header = csv.lines().find(|l| l.starts_with("uid,")).unwrap();
    assert_eq!(
        header.split(',').count(),
        14,
        "uid, generation, username, names_seen, and ten measures"
    );
}

#[test]
fn a_period_resolves_to_utc_boundaries() {
    assert_eq!(
        render::period("2026-09").unwrap(),
        (1_788_220_800, 1_790_812_799)
    );
    assert_eq!(
        render::period("2026-09-26").unwrap(),
        (1_790_380_800, 1_790_467_199)
    );
    // Leap day, and the December rollover into the next year.
    assert_eq!(render::period("2024-02").unwrap().1, 1_709_251_199);
    assert_eq!(
        render::period("2026-12").unwrap(),
        (1_796_083_200, 1_798_761_599)
    );
}

#[test]
fn an_unusable_period_is_refused_rather_than_guessed() {
    for spec in [
        "2026",
        "2026-13",
        "2026-00",
        "nonsense",
        "2026-09-26-01",
        "20xx-09",
    ] {
        assert!(render::period(spec).is_err(), "{spec} must not be accepted");
    }
}

#[test]
fn utc_rendering_matches_known_instants() {
    assert_eq!(render::utc(0), "1970-01-01T00:00:00Z");
    assert_eq!(render::utc(951_782_400), "2000-02-29T00:00:00Z");
    assert_eq!(render::utc(1_709_164_800), "2024-02-29T00:00:00Z");
    assert_eq!(render::utc(4_107_542_400), "2100-03-01T00:00:00Z");
    assert_eq!(render::utc(-1), "1969-12-31T23:59:59Z");
}
