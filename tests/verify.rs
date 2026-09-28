//! Deciding what a result is worth, and refusing to say more than was checked.
//!
//! The tests that matter here are the ones about *not* claiming things. A
//! verifier that overstates is worse than none: it converts "nobody looked" into
//! "it's fine", which is the one output a developer cannot recover from.

use timon::acceptance::Criterion;
use timon::dag_run::{GraphReport, Outcome as TaskOutcome, TaskReport};
use timon::verify::{CheckRun, Checks, Dimension, Outcome, Standing, Subject, verify};

/// A project that declares no checks of its own.
struct NoChecks;
impl Checks for NoChecks {
    fn automated(&self) -> Option<CheckRun> {
        None
    }
}

/// A project whose tests run and pass.
struct PassingChecks(u32);
impl Checks for PassingChecks {
    fn automated(&self) -> Option<CheckRun> {
        Some(CheckRun {
            passed: true,
            tests_run: Some(self.0),
            commands: vec!["cargo test".into()],
            detail: "the suite passed".into(),
        })
    }
}

/// A project whose tests run and fail.
struct FailingChecks;
impl Checks for FailingChecks {
    fn automated(&self) -> Option<CheckRun> {
        Some(CheckRun {
            passed: false,
            tests_run: Some(12),
            commands: vec!["cargo test".into()],
            detail: "2 tests failed".into(),
        })
    }
}

/// A project that can actually decide its acceptance criteria.
struct MechanicalAcceptance(Standing);
impl Checks for MechanicalAcceptance {
    fn automated(&self) -> Option<CheckRun> {
        Some(CheckRun {
            passed: true,
            tests_run: Some(3),
            commands: vec!["cargo test".into()],
            detail: "the suite passed".into(),
        })
    }
    fn acceptance(&self, criteria: &[Criterion]) -> (Standing, String) {
        (
            self.0,
            format!("checked {} criterion/criteria", criteria.len()),
        )
    }
}

fn done(label: &str) -> TaskReport {
    TaskReport {
        label: label.to_string(),
        outcome: TaskOutcome::Done {
            artifact: timon::dag_inputs::Artifact::new(label, 1, "output"),
        },
        input: None,
        started_at: None,
        ended_at: None,
    }
}

fn failed(label: &str, why: &str) -> TaskReport {
    TaskReport {
        label: label.to_string(),
        outcome: TaskOutcome::Failed {
            detail: why.to_string(),
        },
        input: None,
        started_at: None,
        ended_at: None,
    }
}

fn graph(tasks: Vec<TaskReport>) -> GraphReport {
    let complete = tasks.iter().all(|t| t.outcome.finished());
    let not_run = tasks
        .iter()
        .filter(|t| matches!(t.outcome, TaskOutcome::NotRun { .. }))
        .map(|t| t.label.clone())
        .collect();
    GraphReport {
        tasks,
        complete,
        not_run,
    }
}

#[test]
fn a_clean_run_with_no_acceptance_criteria_does_not_pass() {
    // The case this module exists for. Everything ran, everything merged, the
    // tests passed — and nobody checked whether it did what was asked. Saying
    // "pass" here is the lie that matters.
    let execution = graph(vec![done("a"), done("b")]);
    let subject = Subject {
        execution: &execution,
        branch: Some("timon/run-1/result"),
        tested: Some("abc123".into()),
        criteria: Vec::new(),
    };
    let verdict = verify(&subject, &PassingChecks(40));

    assert!(
        !verdict.task_established(),
        "nobody checked the task, so nothing may claim they did"
    );
    let acceptance = verdict
        .judgements
        .iter()
        .find(|j| j.dimension == Dimension::TaskAcceptance)
        .unwrap();
    assert_eq!(acceptance.standing, Standing::NotEstablished);
    assert!(
        acceptance
            .detail
            .contains("no acceptance criteria were written")
    );

    // And the rendering says so where a person will see it.
    let rendered = verdict.render();
    assert!(
        rendered.contains("nobody checked that this did what was asked"),
        "{rendered}"
    );
}

#[test]
fn not_established_is_never_treated_as_a_pass() {
    for standing in [
        Standing::NotEstablished,
        Standing::NotApplicable,
        Standing::Fails,
    ] {
        assert!(
            !standing.affirmative(),
            "{standing:?} must not count as a pass"
        );
    }
    assert!(Standing::Holds.affirmative());
}

#[test]
fn a_failed_task_is_repairable_and_the_defect_is_named() {
    // A repair needs something to aim at.
    let execution = graph(vec![done("a"), failed("b", "the provider refused")]);
    let subject = Subject {
        execution: &execution,
        branch: Some("branch"),
        tested: None,
        criteria: vec![Criterion::FileExists {
            path: "out.txt".into(),
        }],
    };
    let verdict = verify(&subject, &NoChecks);
    match &verdict.outcome {
        Outcome::Repairable { defect } => assert!(defect.contains("b"), "{defect}"),
        other => panic!("expected repairable, got {other:?}"),
    }
}

#[test]
fn failing_project_checks_are_repairable_rather_than_blocked() {
    let execution = graph(vec![done("a")]);
    let subject = Subject {
        execution: &execution,
        branch: Some("branch"),
        tested: None,
        criteria: Vec::new(),
    };
    let verdict = verify(&subject, &FailingChecks);
    match &verdict.outcome {
        Outcome::Repairable { defect } => {
            assert!(defect.contains("2 tests failed"), "{defect}");
        }
        other => panic!("expected repairable, got {other:?}"),
    }
}

#[test]
fn nothing_to_review_is_blocked_not_passed() {
    // A run that produced no branch has produced nothing to judge, and calling
    // that a pass would be the emptiest possible success.
    let execution = graph(vec![done("a")]);
    let subject = Subject {
        execution: &execution,
        branch: None,
        tested: None,
        criteria: Vec::new(),
    };
    let verdict = verify(&subject, &NoChecks);
    assert!(matches!(verdict.outcome, Outcome::Blocked { .. }));
}

#[test]
fn a_pass_requires_acceptance_to_have_been_established() {
    let execution = graph(vec![done("a")]);
    let subject = Subject {
        execution: &execution,
        branch: Some("branch"),
        tested: Some("abc".into()),
        criteria: vec![Criterion::FileExists {
            path: "endpoint.rs".into(),
        }],
    };

    // Established and holding: this is the only shape that passes.
    let verdict = verify(&subject, &MechanicalAcceptance(Standing::Holds));
    assert_eq!(verdict.outcome, Outcome::Pass);
    assert!(verdict.task_established());

    // Established and failing: repairable, with the criterion named.
    let verdict = verify(&subject, &MechanicalAcceptance(Standing::Fails));
    assert!(matches!(verdict.outcome, Outcome::Repairable { .. }));

    // Not established: it does not pass, even though nothing failed.
    let verdict = verify(&subject, &MechanicalAcceptance(Standing::NotEstablished));
    assert!(!verdict.task_established());
}

#[test]
fn the_report_says_how_many_tests_ran() {
    // A repository with few tests gets a weak check, and the number is how a
    // reader knows that rather than having to assume.
    let execution = graph(vec![done("a")]);
    let subject = Subject {
        execution: &execution,
        branch: Some("branch"),
        tested: None,
        criteria: Vec::new(),
    };
    let verdict = verify(&subject, &PassingChecks(3));
    let automated = verdict
        .judgements
        .iter()
        .find(|j| j.dimension == Dimension::AutomatedChecks)
        .unwrap();
    assert!(
        automated.detail.contains("3 test(s) ran"),
        "{}",
        automated.detail
    );
}

#[test]
fn semantic_support_is_reported_as_unchecked_rather_than_omitted() {
    // Whether a claim follows from its source needs judgement, which this does
    // not do. Leaving the dimension out would let a reader assume it was fine.
    let execution = graph(vec![done("a")]);
    let subject = Subject {
        execution: &execution,
        branch: Some("branch"),
        tested: None,
        criteria: Vec::new(),
    };
    let verdict = verify(&subject, &NoChecks);
    let evidence = verdict
        .judgements
        .iter()
        .find(|j| j.dimension == Dimension::Evidence)
        .unwrap();
    assert_eq!(evidence.standing, Standing::NotEstablished);
    assert!(evidence.detail.contains("has not been checked"));
}

#[test]
fn every_dimension_is_judged_on_every_result() {
    // A dimension quietly missing is a dimension a reader will assume was fine.
    let execution = graph(vec![done("a")]);
    let subject = Subject {
        execution: &execution,
        branch: Some("branch"),
        tested: None,
        criteria: Vec::new(),
    };
    let verdict = verify(&subject, &NoChecks);
    for dimension in Dimension::all() {
        assert!(
            verdict.judgements.iter().any(|j| j.dimension == dimension),
            "{dimension:?} was not judged"
        );
    }
}

#[test]
fn a_headline_cannot_be_read_as_more_than_was_checked() {
    let execution = graph(vec![done("a")]);
    let subject = Subject {
        execution: &execution,
        branch: Some("branch"),
        tested: None,
        criteria: Vec::new(),
    };
    let verdict = verify(&subject, &PassingChecks(10));
    let headline = verdict.headline();
    assert!(
        headline.contains("check(s) that were run"),
        "a headline should say what it covers, not imply everything: {headline}"
    );
    assert!(!headline.contains("correct"));
}
