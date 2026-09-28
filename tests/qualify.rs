//! The gate that decides whether a worker may change code.
//!
//! The behaviour worth testing is what happens when the gate is *unsure*. A
//! gate that opens because a probe was broken is the failure mode this exists to
//! prevent, so an unrun check keeps it shut.

use timon::qualify::{Check, Expected, Finding, Observed, Qualification, writing_permitted};

fn finding(check: Check, observed: Observed) -> Finding {
    Finding {
        check,
        expected: check.expected(),
        observed,
        probe: "probe".into(),
        evidence: "evidence".into(),
    }
}

fn all_passing() -> Qualification {
    Qualification {
        findings: Check::all()
            .into_iter()
            .map(|check| {
                let observed = match check.expected() {
                    Expected::Allowed => Observed::Allowed,
                    Expected::Blocked => Observed::Blocked,
                };
                finding(check, observed)
            })
            .collect(),
        host: "test-host".into(),
        taken_at: 1_790_000_000,
    }
}

#[test]
fn a_fully_passing_qualification_permits_writing() {
    let q = all_passing();
    assert!(q.passed());
    assert!(writing_permitted(Some(&q)).is_ok());
}

#[test]
fn a_check_that_could_not_be_run_keeps_the_gate_shut() {
    // An unrun check is an unknown, and unknown is not zero. A gate that opens
    // because a probe was broken is the failure worth designing against.
    let mut q = all_passing();
    q.findings[3].observed = Observed::NotRun;
    assert!(!q.passed());
    let refused = writing_permitted(Some(&q)).unwrap_err();
    assert!(refused.contains("has not qualified"), "{refused}");
}

#[test]
fn a_missing_check_keeps_the_gate_shut_too() {
    // Not just a failing one: a qualification that simply does not mention a
    // check has not made a claim about it.
    let mut q = all_passing();
    q.findings.pop();
    assert!(!q.passed());
}

#[test]
fn never_qualifying_is_not_the_same_as_passing() {
    let refused = writing_permitted(None).unwrap_err();
    assert!(refused.contains("never been qualified"));
    assert!(
        refused.contains("timon qualify write-sandbox"),
        "the refusal should say how to fix it: {refused}"
    );
}

#[test]
fn the_credential_check_is_the_one_this_host_fails() {
    // Measured on 2026-09-28: a worker read the broker's store through the
    // write sandbox. Recorded as a test so the day it starts passing is
    // visible, rather than being noticed by accident.
    let mut q = all_passing();
    q.findings
        .iter_mut()
        .find(|f| f.check == Check::ReadCredentials)
        .unwrap()
        .observed = Observed::Allowed;

    assert!(!q.passed());
    let failures = q.failures();
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].check, Check::ReadCredentials);

    let rendered = q.render();
    assert!(rendered.contains("NOT PASSED"));
    assert!(
        rendered.contains("only holder"),
        "the report should say why it matters: {rendered}"
    );
}

#[test]
fn only_writing_inside_the_worktree_is_supposed_to_be_allowed() {
    // Everything else is a confinement the sandbox must enforce. If this list
    // ever grows an exception, it should be a deliberate edit and not a
    // surprise.
    for check in Check::all() {
        let expected = check.expected();
        if check == Check::WriteInsideWorktree {
            assert_eq!(expected, Expected::Allowed);
        } else {
            assert_eq!(expected, Expected::Blocked, "{check:?}");
        }
    }
}

#[test]
fn every_check_says_why_it_is_a_check() {
    // A gate item nobody can justify gets dropped the first time it is
    // inconvenient.
    for check in Check::all() {
        assert!(check.why().len() > 30, "{check:?}: {}", check.why());
        assert!(!check.as_str().is_empty());
    }
}

#[test]
fn the_recorded_qualification_keeps_every_run_and_now_passes() {
    // The record is evidence, so it has to keep parsing and keep saying what it
    // said. An earlier version of this test asserted the credential check was
    // failing; it fired the moment the store was fixed, which is what it was
    // for. Every run is kept, including the two that did not pass: the first is
    // why the store moved, and deleting it would leave the fix looking
    // unmotivated.
    let raw = std::fs::read_to_string("eval/results/write-sandbox-2026-09-28.json")
        .expect("the qualification record is present");
    let recorded: serde_json::Value = serde_json::from_str(&raw).expect("it parses");

    let runs = recorded["runs"].as_array().expect("runs");
    assert_eq!(runs.len(), 3, "every run is kept, not overwritten");

    let first = &runs[0];
    let credentials = first["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["check"] == "read_credentials")
        .unwrap();
    assert_eq!(
        credentials["observed"], "ALLOWED",
        "the first run is the reason the store moved"
    );

    let latest = runs.last().unwrap();
    let credentials = latest["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["check"] == "read_credentials")
        .unwrap();
    assert_eq!(credentials["observed"], "blocked");
    assert!(
        credentials["evidence"].as_str().unwrap().contains("0700"),
        "the evidence says what makes it blocked, not just that it was"
    );

    assert_eq!(latest["verdict"], "PASSED");
    assert_eq!(
        latest["findings"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|f| f["observed"] == "not_run")
            .count(),
        0,
        "nothing is left unprobed in a run that claims to pass"
    );

    // The one that does not come from the sandbox is marked as such. If a
    // future change removes Timon's process-group reaping, this check would
    // start failing and the note is how somebody learns why.
    let processes = latest["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["check"] == "leave_processes_behind")
        .unwrap();
    assert!(
        processes["note"]
            .as_str()
            .unwrap()
            .contains("NOT the sandbox"),
        "the record says which confinement is Codex's and which is ours"
    );

    // The hook check proved less than its name suggests, and says so.
    let hooks = latest["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["check"] == "run_repository_hooks")
        .unwrap();
    assert!(
        hooks["limit"].as_str().unwrap().contains("does not show"),
        "a check that proved less than its name should say what it did not prove"
    );

    assert_eq!(
        latest["findings"].as_array().unwrap().len(),
        Check::all().len(),
        "one entry per check, so nothing is quietly dropped"
    );

    // The limit is stated rather than left for somebody to discover.
    assert!(
        latest["what_this_does_not_cover"]
            .as_str()
            .unwrap()
            .contains("own logins"),
        "a worker still reads the developer's own credentials, and the record says so"
    );
}
