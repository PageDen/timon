//! How long a run may take, as one number.
//!
//! The property worth testing is the one the old three-knob arrangement did not
//! have: the worst case *is* the budget. A run deadline that only stops work
//! starting, plus a worker deadline that keeps running afterwards, added up to
//! fourteen minutes for a run somebody thought was capped at four.

use std::time::Duration;

use timon::budget::{Budget, DEFAULT_CONCURRENCY, MINIMUM};

#[test]
fn the_worst_case_is_the_budget_not_something_larger() {
    // The whole reason this type exists.
    for secs in [60, 120, 300, 600, 1800] {
        for depth in 1..=6 {
            let budget = Budget::from_total(Duration::from_secs(secs), depth, None);
            assert!(
                budget.worst_case() <= budget.total,
                "{secs}s at depth {depth}: worst case {}s exceeds the budget",
                budget.worst_case().as_secs()
            );
        }
    }
}

#[test]
fn a_deeper_plan_gets_thinner_slices_because_depth_is_what_costs() {
    let shallow = Budget::from_total(Duration::from_secs(300), 2, None);
    let deep = Budget::from_total(Duration::from_secs(300), 5, None);
    assert!(deep.worker_deadline < shallow.worker_deadline);
    // And the planner is counted as a slice, because it is not free. Pretending
    // otherwise is how a budget gets exceeded on its first call.
    assert_eq!(
        shallow.worker_deadline,
        Duration::from_secs(100),
        "300 / (2+1)"
    );
    assert_eq!(deep.worker_deadline, Duration::from_secs(50), "300 / (5+1)");
}

#[test]
fn a_budget_too_small_to_be_worth_starting_is_refused() {
    // Spending tokens and then being stopped is worse than not starting.
    let tiny = Budget::from_total(Duration::from_secs(30), 4, None);
    let why = tiny.viable().unwrap_err();
    assert!(why.contains("below the 60s minimum"), "{why}");
    assert!(why.contains("spend tokens and then"), "it says why: {why}");

    assert!(Budget::from_total(MINIMUM, 1, None).viable().is_ok());
}

#[test]
fn a_depth_that_divides_the_budget_too_finely_is_refused() {
    // 60s across 10 levels is 5s a worker, which is not a model turn.
    let thin = Budget::from_total(Duration::from_secs(60), 10, None);
    let why = thin.viable().unwrap_err();
    assert!(why.contains("not long enough for a model turn"), "{why}");
    assert!(
        why.contains("Allow more time or plan shallower"),
        "and what to do about it: {why}"
    );
}

#[test]
fn a_plan_that_cannot_fit_is_refused_before_anything_is_spent() {
    // The host knows the depth and the per-worker ceiling, so it can say in
    // advance. A plan refused in advance costs nothing; one discovered at
    // minute four costs the whole run.
    let budget = Budget::from_total(Duration::from_secs(300), 4, None);
    assert!(budget.fits(4).is_ok(), "a plan at the stated depth fits");
    assert!(budget.fits(2).is_ok(), "and a shallower one certainly does");

    let why = budget.fits(9).unwrap_err();
    assert!(why.contains("needs up to"), "{why}");
    assert!(
        why.contains("refused before anything was spent"),
        "the refusal says when it happened: {why}"
    );
}

#[test]
fn width_is_not_the_budgets_business() {
    // Independent tasks overlap, so concurrency is about how much of a shared
    // host one run should take — not about how long it lasts.
    let narrow = Budget::from_total(Duration::from_secs(300), 4, Some(1));
    let wide = Budget::from_total(Duration::from_secs(300), 4, Some(8));
    assert_eq!(narrow.worker_deadline, wide.worker_deadline);
    assert_eq!(narrow.worst_case(), wide.worst_case());
    assert_eq!(narrow.concurrency, 1);
    assert_eq!(wide.concurrency, 8);

    // And it never lands on zero, which would run nothing at all.
    assert_eq!(
        Budget::from_total(Duration::from_secs(300), 4, Some(0)).concurrency,
        1
    );
    assert_eq!(
        Budget::from_total(Duration::from_secs(300), 4, None).concurrency,
        DEFAULT_CONCURRENCY
    );
}

#[test]
fn the_derivation_is_visible_rather_than_implied() {
    // Somebody has to be able to see where the numbers came from, or the one
    // knob is just a different way to be surprised.
    let budget = Budget::from_total(Duration::from_secs(300), 4, None);
    let described = budget.describe();
    assert!(described.contains("300s total"));
    assert!(
        described.contains("60s"),
        "the per-worker slice: {described}"
    );
    assert!(described.contains("Worst case 300s"), "{described}");
}

#[test]
fn a_five_minute_budget_lands_where_a_person_would_expect() {
    // The question that prompted this: what does "three to five minutes" buy.
    let budget = Budget::from_total(Duration::from_secs(300), 4, None);
    assert_eq!(budget.worker_deadline, Duration::from_secs(60));
    assert_eq!(budget.run_deadline, Duration::from_secs(240));
    assert_eq!(budget.worst_case(), Duration::from_secs(300));
    // Four levels of work plus the planner, a minute each.
    assert!(budget.fits(4).is_ok());
}
