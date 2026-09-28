//! Choosing how a goal is run.
//!
//! The pilot's finding is the whole basis of this module: orchestrating
//! everything cost 1.365x asking the strong model directly, because the lead's
//! two calls cost ~30,000 tokens whatever the work was. So the tests that matter
//! are the ones about *not* orchestrating, and about not letting weak evidence
//! push work upward.

use timon::triage::{Allowances, FAST_PATH_MAX_CHARS, Route, decide, measure};

fn open() -> Allowances {
    Allowances {
        route: None,
        allow_planner: true,
    }
}

fn fired(decision: &timon::triage::Decision, rule: &str) -> bool {
    decision.reasons.iter().any(|reason| reason.rule == rule)
}

#[test]
fn small_self_contained_work_takes_the_cheap_route() {
    let decision = decide("Summarise the changelog for the last release.", &open());
    assert_eq!(decision.route, Route::CheapWorker);
    assert!(fired(&decision, "fast_path_eligible"));
}

#[test]
fn work_needing_tools_the_cheap_route_lacks_goes_to_one_strong_call() {
    // A capability fact, not a guess about difficulty: a single cheap read-only
    // call cannot change code or run tests.
    for goal in [
        "Refactor the retention module.",
        "Implement a --since flag.",
        "Fix the failing test in recorder.",
        "Benchmark the relay loop.",
    ] {
        let decision = decide(goal, &open());
        assert_eq!(decision.route, Route::StrongWorker, "for {goal:?}");
        assert!(fired(&decision, "needs_tools"), "for {goal:?}");
    }
}

#[test]
fn a_long_goal_is_kept_out_of_the_fast_path_but_not_pushed_to_the_planner() {
    // The distinction the plan insists on: length is weak evidence, so it can
    // only disqualify from the cheapest route. It must never raise one.
    let long = "x".repeat(FAST_PATH_MAX_CHARS + 1);
    let decision = decide(&long, &open());
    assert_eq!(
        decision.route,
        Route::StrongWorker,
        "length must not reach the planner on its own"
    );
    assert!(fired(&decision, "goal_too_long"));
}

#[test]
fn a_short_goal_can_still_be_hard_and_that_is_not_triages_business() {
    // "A short migration request can be difficult." Triage does not pretend to
    // know; it only checks whether the fast path has the tools.
    let decision = decide("Migrate the schema to v3.", &open());
    assert_eq!(decision.route, Route::StrongWorker);
    assert!(fired(&decision, "needs_tools"));
    assert!(decision.signals.goal_chars < FAST_PATH_MAX_CHARS);
}

#[test]
fn several_deliverables_reach_the_planner_only_when_it_is_allowed() {
    let goal = "Summarise the changelog; then draft the release notes";

    let decision = decide(goal, &open());
    assert_eq!(decision.route, Route::Planner);
    assert!(fired(&decision, "several_deliverables"));

    let closed = Allowances {
        route: None,
        allow_planner: false,
    };
    let decision = decide(goal, &closed);
    assert_eq!(
        decision.route,
        Route::StrongWorker,
        "without permission the most that happens is one strong call"
    );
    assert!(fired(&decision, "planner_not_allowed"));
}

#[test]
fn the_caller_s_own_choice_wins_and_is_recorded_as_theirs() {
    // So a later evaluation does not credit or blame these rules for a route
    // nobody here chose.
    let insisted = Allowances {
        route: Some(Route::CheapWorker),
        allow_planner: true,
    };
    let decision = decide("Refactor everything; and also rewrite the docs", &insisted);
    assert_eq!(decision.route, Route::CheapWorker);
    assert!(fired(&decision, "caller_chose"));
    assert_eq!(
        decision.reasons.len(),
        1,
        "no other rule should claim credit"
    );
}

#[test]
fn every_decision_carries_the_reasons_that_produced_it() {
    // Misrouting is a question about the alternative that was not run. Nothing
    // downstream can answer it, so the evidence has to survive here.
    let decision = decide(
        "Implement a --since flag with a very long explanation "
            .repeat(10)
            .as_str(),
        &open(),
    );
    assert!(decision.reasons.len() >= 2, "{:?}", decision.reasons);
    assert!(fired(&decision, "needs_tools"));
    assert!(fired(&decision, "goal_too_long"));
    for reason in &decision.reasons {
        assert!(!reason.rule.is_empty());
        assert!(
            reason.detail.len() > 10,
            "a reason an operator cannot read is not a reason: {reason:?}"
        );
    }
}

#[test]
fn signals_are_measured_not_judged() {
    let signals = measure("Summarise this; then do that");
    assert_eq!(signals.deliverables, 2);
    assert!(signals.needs_tools.is_empty());

    let signals = measure("Refactor and migrate the parser");
    assert_eq!(
        signals.needs_tools,
        vec!["changing code across files".to_string()],
        "two phrases meaning the same capability are one reason, not two"
    );
}

#[test]
fn what_a_route_costs_is_stated_in_the_unit_budgets_use() {
    // "One cheap-model call" was ambiguous between one provider request and one
    // worker session. Left ambiguous it would make every cost comparison in the
    // evaluation unreadable.
    assert!(
        Route::CheapWorker
            .sessions()
            .contains("one cheap worker session")
    );
    assert!(
        Route::StrongWorker
            .sessions()
            .contains("one strong worker session")
    );
    assert!(Route::Planner.sessions().contains("per task"));
}

#[test]
fn a_route_round_trips_through_its_text_form() {
    for route in [Route::CheapWorker, Route::StrongWorker, Route::Planner] {
        assert_eq!(Route::parse(route.as_str()), Some(route));
    }
    assert_eq!(Route::parse("cheap"), Some(Route::CheapWorker));
    assert_eq!(Route::parse("nonsense"), None);
}

#[test]
fn a_decision_is_recorded_against_its_run_and_read_back() {
    use timon::run::record::{Base, Runs};
    use timon::run::start::{Request, admit};

    let dir = tempfile::tempdir().unwrap();
    let runs = Runs::open(dir.path().join("runs.sqlite")).unwrap();
    let run = admit(
        &runs,
        Request {
            goal: "Summarise the changelog.".to_string(),
            principal_uid: 1000,
            workspace: None,
            accounts: Vec::new(),
            max_attempts: 8,
            token_ceiling: None,
            deadline: None,
            submission_key: None,
        },
        Base::None,
        1_790_000_000,
        None,
        20_000,
    )
    .unwrap();

    let decision = decide(&run.goal, &open());
    runs.record_triage(&run.id, &decision, 1_790_000_001)
        .unwrap();

    let read = runs.triage_of(&run.id).unwrap();
    assert_eq!(read.len(), 1);
    assert_eq!(read[0].route, Route::CheapWorker);
    assert_eq!(read[0].reasons, decision.reasons);
    assert_eq!(read[0].signals.goal_chars, decision.signals.goal_chars);
}

#[test]
fn overlapping_separators_do_not_inflate_the_deliverable_count() {
    // "…this; then that" contains both "; " and " then ". Counting matches made
    // two deliverables look like three — and over-counting is the dangerous
    // direction, because the count is the one signal allowed to raise a route.
    assert_eq!(measure("Summarise this; then do that").deliverables, 2);
    assert_eq!(measure("Do one thing").deliverables, 1);
    assert_eq!(measure("A; B; C").deliverables, 3);
    assert_eq!(
        measure("Trailing separator; ").deliverables,
        1,
        "an empty tail is not a deliverable"
    );
    assert_eq!(
        measure("- first\n- second\n- third").deliverables,
        3,
        "a list is what several deliverables usually looks like"
    );
}
