//! Per-run admission: what it enforces, what it only estimates, and the
//! difference being visible in what it reports.

use std::path::Path;

use timon::admission::{CEILING_BASIS, Ledger, Refusal, RunLimits};

fn ledger(dir: &Path) -> Ledger {
    Ledger::open(&dir.join("runs"), "run-1").unwrap()
}

fn limits(max_attempts: u32, ceiling: Option<u64>, reserve: u64) -> RunLimits {
    RunLimits {
        max_attempts,
        token_ceiling: ceiling,
        attempt_reserve: reserve,
    }
}

#[test]
fn an_attempt_count_is_enforced_because_the_ledger_is_where_attempts_start() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = ledger(dir.path());
    let limits = limits(3, None, 100);

    for n in 0..3 {
        assert!(ledger.admit(&format!("a{n}"), &limits).unwrap().is_ok());
    }
    let refused = ledger.admit("a3", &limits).unwrap().unwrap_err();

    assert_eq!(
        refused,
        Refusal::AttemptsExhausted {
            started: 3,
            max_attempts: 3
        }
    );
}

#[test]
fn a_ceiling_refuses_the_attempt_that_would_pass_it() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = ledger(dir.path());
    let limits = limits(100, Some(250), 100);

    assert!(ledger.admit("a0", &limits).unwrap().is_ok());
    assert!(ledger.admit("a1", &limits).unwrap().is_ok());
    let refused = ledger.admit("a2", &limits).unwrap().unwrap_err();

    match refused {
        Refusal::CeilingWouldBeExceeded {
            committed,
            requested,
            ceiling,
            ..
        } => {
            assert_eq!((committed, requested, ceiling), (200, 100, 250));
        }
        other => panic!("expected a ceiling refusal, got {other:?}"),
    }
}

#[test]
fn settling_below_the_reservation_frees_room_for_another_attempt() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = ledger(dir.path());
    let limits = limits(100, Some(250), 100);
    ledger.admit("a0", &limits).unwrap().unwrap();
    ledger.admit("a1", &limits).unwrap().unwrap();
    assert!(ledger.admit("a2", &limits).unwrap().is_err());

    // Both attempts turn out to have spent far less than was held for them.
    ledger.settle("a0", Some(10)).unwrap();
    ledger.settle("a1", Some(10)).unwrap();

    assert!(
        ledger.admit("a2", &limits).unwrap().is_ok(),
        "a reservation is a guess; the real number replaces it"
    );
}

#[test]
fn an_attempt_whose_usage_never_arrived_keeps_its_reservation() {
    // The whole point. An attempt that reported nothing may have spent anything,
    // and settling it to zero would let the run admit work it cannot account for.
    let dir = tempfile::tempdir().unwrap();
    let ledger = ledger(dir.path());
    let limits = limits(100, Some(250), 100);
    ledger.admit("a0", &limits).unwrap().unwrap();
    ledger.admit("a1", &limits).unwrap().unwrap();

    let settled = ledger.settle("a0", None).unwrap();

    assert!(settled.known);
    assert!(!settled.usage_known);
    assert_eq!(settled.committed, 200, "unknown must not settle to zero");
    assert!(
        ledger.admit("a2", &limits).unwrap().is_err(),
        "and the run stays refused, as it should while the tokens are unaccounted for"
    );
}

#[test]
fn a_refusal_says_how_much_is_held_for_attempts_nobody_can_account_for() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = ledger(dir.path());
    let generous = limits(100, Some(250), 100);
    ledger.admit("a0", &generous).unwrap().unwrap();
    ledger.admit("a1", &generous).unwrap().unwrap();
    ledger.settle("a0", Some(10)).unwrap();

    // a1 is still running, so 100 of the committed total is a guess.
    let tight = limits(100, Some(150), 100);
    let refused = ledger.admit("a2", &tight).unwrap().unwrap_err();

    match refused {
        Refusal::CeilingWouldBeExceeded {
            committed,
            unsettled,
            ..
        } => {
            assert_eq!(committed, 110);
            assert_eq!(
                unsettled, 100,
                "a reader has to be able to tell a real charge from a held guess"
            );
        }
        other => panic!("expected a ceiling refusal, got {other:?}"),
    }
}

#[test]
fn readmitting_the_same_attempt_id_does_not_charge_the_run_twice() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = ledger(dir.path());
    let limits = limits(100, Some(1_000), 100);

    let first = ledger.admit("a0", &limits).unwrap().unwrap();
    let again = ledger.admit("a0", &limits).unwrap().unwrap();

    assert_eq!(first.committed_after, again.committed_after);
    assert_eq!(
        ledger.summary().unwrap().attempts,
        1,
        "one attempt, one entry"
    );
}

#[test]
fn settling_the_same_attempt_twice_does_not_change_what_it_cost() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = ledger(dir.path());
    let limits = limits(100, Some(1_000), 100);
    ledger.admit("a0", &limits).unwrap().unwrap();

    ledger.settle("a0", Some(42)).unwrap();
    let repeat = ledger.settle("a0", Some(999)).unwrap();

    assert_eq!(
        repeat.committed, 42,
        "a retried report must not restate the charge"
    );
}

#[test]
fn two_processes_admitting_at_once_cannot_both_take_the_last_room() {
    // The race the lock exists for: both read, both see room, both admit.
    let dir = tempfile::tempdir().unwrap();
    let runs = dir.path().join("runs");
    std::fs::create_dir_all(&runs).unwrap();
    let limits = limits(100, Some(100), 100);

    let handles: Vec<_> = (0..8)
        .map(|n| {
            let runs = runs.clone();
            std::thread::spawn(move || {
                let ledger = Ledger::open(&runs, "run-1").unwrap();
                ledger.admit(&format!("a{n}"), &limits).unwrap().is_ok()
            })
        })
        .collect();
    let admitted = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .filter(|was_admitted| *was_admitted)
        .count();

    assert_eq!(admitted, 1, "exactly one attempt fits in a ceiling of 100");
    let summary = Ledger::open(&runs, "run-1").unwrap().summary().unwrap();
    assert_eq!(summary.attempts, 1);
    assert_eq!(summary.committed, 100);
}

#[test]
fn a_run_without_a_ceiling_is_limited_only_by_its_attempt_count() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = ledger(dir.path());
    let limits = limits(5, None, 1_000_000);

    for n in 0..5 {
        assert!(ledger.admit(&format!("a{n}"), &limits).unwrap().is_ok());
    }
    assert!(ledger.admit("a5", &limits).unwrap().is_err());
}

#[test]
fn a_zero_reservation_is_refused_because_it_would_admit_without_limit() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = ledger(dir.path());
    let limits = limits(100, Some(10), 0);

    for n in 0..11 {
        let _ = ledger.admit(&format!("a{n}"), &limits);
    }

    let summary = ledger.summary().unwrap();
    assert!(
        summary.attempts <= 10,
        "a reservation of zero must not let a ceiling of 10 admit forever, got {} attempts",
        summary.attempts
    );
}

#[test]
fn the_ledger_survives_the_process_that_wrote_it() {
    let dir = tempfile::tempdir().unwrap();
    let runs = dir.path().join("runs");
    let limits = limits(100, Some(1_000), 100);
    Ledger::open(&runs, "run-1")
        .unwrap()
        .admit("a0", &limits)
        .unwrap()
        .unwrap();

    // A separate handle, as a second process would have.
    let reopened = Ledger::open(&runs, "run-1").unwrap();
    assert_eq!(reopened.summary().unwrap().attempts, 1);
    assert_eq!(reopened.summary().unwrap().committed, 100);
}

#[test]
fn runs_do_not_share_a_ledger() {
    let dir = tempfile::tempdir().unwrap();
    let runs = dir.path().join("runs");
    let limits = limits(1, None, 100);
    Ledger::open(&runs, "run-a")
        .unwrap()
        .admit("a0", &limits)
        .unwrap()
        .unwrap();

    assert!(
        Ledger::open(&runs, "run-b")
            .unwrap()
            .admit("a0", &limits)
            .unwrap()
            .is_ok(),
        "one run using its allowance must not refuse another run"
    );
}

#[test]
fn a_ceiling_is_never_reported_as_a_cap() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = ledger(dir.path());
    let admitted = ledger
        .admit("a0", &limits(10, Some(1_000), 100))
        .unwrap()
        .unwrap();

    for text in [
        admitted.basis,
        ledger.summary().unwrap().basis,
        CEILING_BASIS,
    ] {
        assert!(text.contains("not a cap"));
        assert!(text.contains("Overshoot is possible"));
        assert!(
            !text.to_lowercase().contains("guarantee"),
            "nothing here may read as a guarantee"
        );
    }
}

#[test]
fn the_ledger_and_its_directory_are_private() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let runs = dir.path().join("runs");
    let ledger = Ledger::open(&runs, "run-1").unwrap();
    ledger.admit("a0", &limits(10, None, 100)).unwrap().unwrap();

    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&runs), 0o700);
    assert_eq!(mode(&runs.join("run-run-1.json")), 0o600);
}

#[test]
fn a_settled_unknown_attempt_is_still_settled_after_the_ledger_is_reloaded() {
    // `Some(None)` and `None` are different states -- finished with usage
    // unknown, versus still running -- and a serialisation that collapses both
    // to null would let a later report overwrite a settled charge.
    let dir = tempfile::tempdir().unwrap();
    let runs = dir.path().join("runs");
    let limits = limits(10, Some(10_000), 1_000);
    Ledger::open(&runs, "run-1")
        .unwrap()
        .admit("a0", &limits)
        .unwrap()
        .unwrap();
    Ledger::open(&runs, "run-1")
        .unwrap()
        .settle("a0", None)
        .unwrap();

    // A later process reports a number for an attempt already settled unknown.
    let reopened = Ledger::open(&runs, "run-1").unwrap();
    let after = reopened.settle("a0", Some(5)).unwrap();

    assert_eq!(
        after.committed, 1_000,
        "a settled-unknown attempt keeps its reservation across a reload"
    );
    assert_eq!(
        reopened.summary().unwrap().attempts_with_unknown_usage,
        1,
        "and is still counted as unaccounted for"
    );
}
