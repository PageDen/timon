//! Choosing an account, and the two things that make it non-obvious.
//!
//! Accounts are not interchangeable — one account in the pilot pool advertised a
//! model in its catalog and was then refused that model — and a conversation
//! cannot be moved between them once it has started. These tests pin both, plus
//! the quota reading the choice is ranked on.

use serde_json::json;
use timon::broker::quota::{self, Standing};
use timon::broker::select::{NoAccount, Pool, model_of, thread_of};
use timon::broker::serve::{model_unavailable, status_of};

const NOW: i64 = 1_790_000_000;

/// Two different Linux logins on the shared host.
const ALICE: u32 = 1000;
const BOB: u32 = 1001;

fn standing(account: &str, used_percent: u32) -> Standing {
    Standing {
        account_id: account.to_string(),
        email: None,
        plan_type: Some("pro".to_string()),
        allowed: true,
        limit_reached: false,
        used_percent: Some(used_percent),
        reset_at: Some(NOW + 600),
        read_at: NOW,
    }
}

fn exhausted(account: &str) -> Standing {
    Standing {
        allowed: false,
        limit_reached: true,
        used_percent: Some(100),
        ..standing(account, 100)
    }
}

fn pool_of(readings: &[(&str, Standing)]) -> Pool {
    let mut pool = Pool::new();
    for (name, standing) in readings {
        pool.observe(name, standing.clone());
    }
    pool
}

fn names(list: &[&str]) -> Vec<String> {
    list.iter().map(|name| name.to_string()).collect()
}

#[test]
fn the_account_with_the_most_of_its_window_left_goes_first() {
    let mut pool = pool_of(&[
        ("acct2", standing("acct2", 80)),
        ("acct3", standing("acct3", 4)),
    ]);
    let chosen = pool
        .choose(&names(&["acct2", "acct3"]), None, None, &[], NOW)
        .unwrap();
    assert_eq!(chosen, "acct3");
}

#[test]
fn an_account_never_measured_is_still_eligible() {
    // A missing reading means the usage endpoint has not been asked, which is not
    // evidence of exhaustion. The broker must not depend on an undocumented
    // endpoint being up in order to serve anything at all.
    let mut pool = pool_of(&[("acct2", standing("acct2", 99))]);
    let chosen = pool
        .choose(&names(&["acct2", "acct3"]), None, None, &[], NOW)
        .unwrap();
    assert_eq!(
        chosen, "acct3",
        "the unmeasured account outranks a nearly full one"
    );
}

#[test]
fn a_thread_stays_on_the_account_that_started_it_even_when_another_has_more_room() {
    let mut pool = pool_of(&[
        ("acct2", standing("acct2", 90)),
        ("acct3", standing("acct3", 1)),
    ]);
    pool.bind(ALICE, "thread-a", "acct2", NOW);
    let chosen = pool
        .choose(
            &names(&["acct2", "acct3"]),
            None,
            Some((ALICE, "thread-a")),
            &[],
            NOW,
        )
        .unwrap();
    assert_eq!(
        chosen, "acct2",
        "continuity must beat headroom: the provider holds state against the thread"
    );
}

#[test]
fn an_affinity_lapses_so_the_map_cannot_grow_without_bound() {
    let mut pool = pool_of(&[
        ("acct2", standing("acct2", 90)),
        ("acct3", standing("acct3", 1)),
    ]);
    pool.bind(ALICE, "thread-a", "acct2", NOW);
    let later = NOW + 13 * 3600;
    let chosen = pool
        .choose(
            &names(&["acct2", "acct3"]),
            None,
            Some((ALICE, "thread-a")),
            &[],
            later,
        )
        .unwrap();
    assert_eq!(chosen, "acct3");
    assert!(pool.bound(ALICE, "thread-a", later).is_none());
}

#[test]
fn a_thread_bound_to_an_exhausted_account_moves_rather_than_failing() {
    let mut pool = pool_of(&[
        ("acct2", exhausted("acct2")),
        ("acct3", standing("acct3", 50)),
    ]);
    pool.bind(ALICE, "thread-a", "acct2", NOW);
    let chosen = pool
        .choose(
            &names(&["acct2", "acct3"]),
            None,
            Some((ALICE, "thread-a")),
            &[],
            NOW,
        )
        .unwrap();
    assert_eq!(chosen, "acct3");
}

#[test]
fn an_account_that_refused_a_model_is_not_offered_that_model_again() {
    // The pilot's Go-plan account listed gpt-5.5 in its catalog and would not
    // serve it. Remembering the refusal is what stops every request retrying it.
    let mut pool = pool_of(&[
        ("acct3", standing("acct3", 1)),
        ("acct2", standing("acct2", 70)),
    ]);
    pool.refused("acct3", "gpt-5.5", NOW);
    let chosen = pool
        .choose(&names(&["acct3", "acct2"]), Some("gpt-5.5"), None, &[], NOW)
        .unwrap();
    assert_eq!(chosen, "acct2", "despite acct3 having far more headroom");

    // A different model is unaffected: the exclusion is per account and model.
    let chosen = pool
        .choose(
            &names(&["acct3", "acct2"]),
            Some("gpt-5.6-luna"),
            None,
            &[],
            NOW,
        )
        .unwrap();
    assert_eq!(chosen, "acct3");
}

#[test]
fn a_model_refusal_expires_so_a_new_entitlement_is_picked_up() {
    let mut pool = pool_of(&[("acct3", standing("acct3", 1))]);
    pool.refused("acct3", "gpt-5.5", NOW);
    assert!(pool.refuses("acct3", "gpt-5.5", NOW + 10));
    assert!(!pool.refuses("acct3", "gpt-5.5", NOW + 3601));
}

#[test]
fn every_account_exhausted_is_refused_as_temporary_not_as_a_client_error() {
    let mut pool = pool_of(&[("acct2", exhausted("acct2")), ("acct3", exhausted("acct3"))]);
    let why = pool
        .choose(&names(&["acct2", "acct3"]), None, None, &[], NOW)
        .unwrap_err();
    assert_eq!(why, NoAccount::AllExhausted);
    assert!(format!("{why}").contains("not forwarded"));
}

#[test]
fn a_model_no_account_serves_is_named_in_the_refusal() {
    let mut pool = pool_of(&[
        ("acct2", standing("acct2", 5)),
        ("acct3", standing("acct3", 5)),
    ]);
    pool.refused("acct2", "gpt-5.5", NOW);
    pool.refused("acct3", "gpt-5.5", NOW);
    let why = pool
        .choose(&names(&["acct2", "acct3"]), Some("gpt-5.5"), None, &[], NOW)
        .unwrap_err();
    assert_eq!(
        why,
        NoAccount::NoneServesModel {
            model: "gpt-5.5".to_string()
        }
    );
    assert!(format!("{why}").contains("gpt-5.5"));
}

#[test]
fn an_empty_pool_is_distinguished_from_an_exhausted_one() {
    // The operator's next action differs: one needs an account added, the other
    // needs a window to reset.
    let mut pool = Pool::new();
    assert_eq!(
        pool.choose(&[], None, None, &[], NOW).unwrap_err(),
        NoAccount::PoolEmpty
    );
}

#[test]
fn an_account_already_tried_on_this_request_is_not_tried_again() {
    let mut pool = pool_of(&[
        ("acct2", standing("acct2", 1)),
        ("acct3", standing("acct3", 90)),
    ]);
    let chosen = pool
        .choose(
            &names(&["acct2", "acct3"]),
            None,
            None,
            &names(&["acct2"]),
            NOW,
        )
        .unwrap();
    assert_eq!(chosen, "acct3");
}

#[test]
fn the_same_pool_and_the_same_readings_always_choose_the_same_account() {
    // Ties break on store order rather than on hash iteration order, so a report
    // of what the broker did is reproducible.
    for _ in 0..20 {
        let mut pool = pool_of(&[
            ("acct2", standing("acct2", 20)),
            ("acct3", standing("acct3", 20)),
        ]);
        let chosen = pool
            .choose(&names(&["acct2", "acct3"]), None, None, &[], NOW)
            .unwrap();
        assert_eq!(chosen, "acct2");
    }
}

#[test]
fn the_thread_header_codex_actually_sends_is_the_one_read() {
    // Captured from codex_exec 0.155.1 against a local listener: `thread-id` and
    // `session-id` carry the same value, and `x-codex-window-id` is `thread:window`.
    let headers = vec![
        ("session-id".to_string(), "01a0e6a2-b674".to_string()),
        ("thread-id".to_string(), "01a0e6a2-b674".to_string()),
        (
            "x-codex-window-id".to_string(),
            "01a0e6a2-b674:0".to_string(),
        ),
    ];
    assert_eq!(thread_of(&headers).unwrap(), "01a0e6a2-b674");

    // Only the window id present: its thread part is used, not the whole value.
    let window = vec![(
        "x-codex-window-id".to_string(),
        "01a0e6a2-b674:3".to_string(),
    )];
    assert_eq!(thread_of(&window).unwrap(), "01a0e6a2-b674");

    assert!(thread_of(&[]).is_none());
    let empty = vec![("thread-id".to_string(), "  ".to_string())];
    assert!(
        thread_of(&empty).is_none(),
        "a blank header is not a thread"
    );
}

#[test]
fn a_body_that_is_not_json_does_not_stop_the_request() {
    assert_eq!(
        model_of(br#"{"model":"gpt-5.5","stream":true}"#).unwrap(),
        "gpt-5.5"
    );
    assert!(model_of(b"not json at all").is_none());
    assert!(model_of(b"{").is_none());
    assert!(model_of(br#"{"stream":true}"#).is_none());
    assert!(model_of(br#"{"model":""}"#).is_none());
    assert!(model_of(b"").is_none());
}

#[test]
fn a_model_refusal_is_told_apart_from_a_mistyped_path() {
    assert!(model_unavailable(
        404,
        br#"{"error":{"message":"The model `gpt-5.5` does not exist","code":"model_not_found"}}"#
    ));
    assert!(model_unavailable(
        404,
        br#"{"error":{"message":"Model not found"}}"#
    ));
    assert!(model_unavailable(
        400,
        br#"{"error":{"message":"Your account does not have access to model gpt-5.5"}}"#
    ));
    // A 404 from a wrong path must not rotate the pool: four identical failures
    // would hide the typo instead of reporting it.
    assert!(!model_unavailable(404, b"<html>404 Not Found</html>"));
    assert!(!model_unavailable(
        500,
        br#"{"error":{"message":"model not found"}}"#
    ));
    assert!(!model_unavailable(429, b"rate limited"));
}

#[test]
fn a_status_line_is_read_out_of_a_response_head() {
    assert_eq!(
        status_of(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n"),
        Some(200)
    );
    assert_eq!(
        status_of(b"HTTP/1.1 429 Too Many Requests\r\n\r\n"),
        Some(429)
    );
    assert_eq!(status_of(b"garbage\r\n\r\n"), None);
    assert_eq!(status_of(b""), None);
}

#[test]
fn the_usage_shape_the_provider_actually_returns_is_read_correctly() {
    // Captured from chatgpt.com/backend-api/wham/usage for a pooled account.
    let body = json!({
        "user_id": "user-XXXX",
        "account_id": "7d5a5039-9914-431f-8f3e-1d1061ae2461",
        "email": "pooled@example.com",
        "plan_type": "pro",
        "rate_limit": {
            "allowed": true,
            "limit_reached": false,
            "primary_window": {
                "used_percent": 4,
                "limit_window_seconds": 604_800,
                "reset_after_seconds": 513_778,
                "reset_at": 1_791_089_221i64
            },
            "secondary_window": null
        },
        "credits": {"has_credits": false, "unlimited": false, "balance": "0"},
        "spend_control": {"reached": false, "individual_limit": null}
    });
    let standing = quota::parse(&body, "fallback", NOW);
    assert_eq!(standing.account_id, "7d5a5039-9914-431f-8f3e-1d1061ae2461");
    assert_eq!(standing.plan_type.as_deref(), Some("pro"));
    assert_eq!(standing.used_percent, Some(4));
    assert_eq!(standing.reset_at, Some(1_791_089_221));
    assert!(standing.usable());
    assert_eq!(standing.headroom(), 96);
}

#[test]
fn the_provider_saying_no_is_trusted_over_the_percentage() {
    // A window can read low and still be refused, for instance by a spend
    // control. `allowed` is the provider's answer and decides.
    let body = json!({
        "rate_limit": {
            "allowed": false,
            "limit_reached": false,
            "primary_window": {"used_percent": 2, "reset_at": 1_791_089_221i64}
        }
    });
    let standing = quota::parse(&body, "acct2", NOW);
    assert!(!standing.usable());
}

#[test]
fn a_usage_reply_missing_its_fields_degrades_rather_than_disabling_the_account() {
    // The endpoint is undocumented. A field that moves must not take the pool
    // offline; it must only make the reading less informative.
    let standing = quota::parse(&json!({}), "acct2", NOW);
    assert_eq!(standing.account_id, "acct2");
    assert!(standing.usable(), "an unreadable reply is not a refusal");
    assert_eq!(standing.used_percent, None);
    assert_eq!(standing.headroom(), 50, "unknown usage ranks mid-pool");
}

#[test]
fn a_reading_goes_stale_so_usage_spent_elsewhere_is_noticed() {
    let standing = standing("acct2", 4);
    assert!(!standing.stale(NOW + 30));
    assert!(standing.stale(NOW + 61));
}

#[test]
fn two_callers_sending_the_same_thread_id_do_not_share_a_binding() {
    // Raised by Codex's review of the plan, and it was right. The thread id
    // arrives in a header, so it is whatever the caller types. Keyed on the
    // thread alone, Bob would inherit Alice's account — and could park his own
    // conversation on an account by naming a thread that is not his.
    let mut pool = pool_of(&[
        ("acct2", standing("acct2", 90)),
        ("acct3", standing("acct3", 1)),
    ]);
    pool.bind(ALICE, "shared-thread-id", "acct2", NOW);

    let alice = pool
        .choose(
            &names(&["acct2", "acct3"]),
            None,
            Some((ALICE, "shared-thread-id")),
            &[],
            NOW,
        )
        .unwrap();
    assert_eq!(alice, "acct2", "Alice keeps her own binding");

    let bob = pool
        .choose(
            &names(&["acct2", "acct3"]),
            None,
            Some((BOB, "shared-thread-id")),
            &[],
            NOW,
        )
        .unwrap();
    assert_eq!(
        bob, "acct3",
        "Bob is chosen on headroom, not handed Alice's account"
    );
    assert!(pool.bound(BOB, "shared-thread-id", NOW).is_none());
    assert_eq!(pool.bound(ALICE, "shared-thread-id", NOW), Some("acct2"));
}

#[test]
fn one_caller_keeps_its_own_threads_apart() {
    let mut pool = pool_of(&[
        ("acct2", standing("acct2", 50)),
        ("acct3", standing("acct3", 50)),
    ]);
    pool.bind(ALICE, "thread-one", "acct3", NOW);
    assert_eq!(pool.bound(ALICE, "thread-one", NOW), Some("acct3"));
    assert!(
        pool.bound(ALICE, "thread-two", NOW).is_none(),
        "a second conversation is not captured by the first"
    );
}
