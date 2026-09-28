//! The task graph, and the rule that makes a dependency mean something.
//!
//! The acceptance Codex's review asked for is the last test here: A defines an
//! interface and B sees *that exact interface*. Proving only that A ran before B
//! is what the first draft of this plan did, and it guarantees nothing.

use std::collections::{BTreeMap, BTreeSet};

use timon::dag::{Access, Invalid, Limits, Plan, Task, validate};
use timon::dag_inputs::{Artifact, Consumed, Input, InputError, inputs_for, stale};

fn task(label: &str, depends_on: &[&str]) -> Task {
    Task {
        label: label.to_string(),
        task: format!("do {label}"),
        depends_on: depends_on.iter().map(|s| s.to_string()).collect(),
        access: Access::Read,
    }
}

fn plan(tasks: Vec<Task>) -> Plan {
    Plan {
        tasks,
        notes: String::new(),
    }
}

#[test]
fn a_valid_plan_orders_dependencies_before_their_dependants() {
    let valid = validate(
        &plan(vec![
            task("screen", &["api"]),
            task("api", &[]),
            task("docs", &["api", "screen"]),
        ]),
        &Limits::default(),
    )
    .expect("valid");

    let order = valid.order();
    let at = |label: &str| order.iter().position(|l| l == label).unwrap();
    assert!(at("api") < at("screen"));
    assert!(at("screen") < at("docs"));
    assert_eq!(valid.depth(), 3);
}

#[test]
fn the_same_plan_always_orders_the_same_way() {
    // A scheduler whose order depends on hash iteration is one nobody can
    // reproduce, and a report of what ran becomes unrepeatable.
    let build = || {
        validate(
            &plan(vec![task("c", &[]), task("a", &[]), task("b", &[])]),
            &Limits::default(),
        )
        .unwrap()
        .order()
        .to_vec()
    };
    let first = build();
    for _ in 0..20 {
        assert_eq!(build(), first);
    }
    assert_eq!(first, vec!["a", "b", "c"]);
}

#[test]
fn a_cycle_is_refused_and_names_the_tasks_caught_in_it() {
    let faults = validate(
        &plan(vec![
            task("a", &["c"]),
            task("b", &["a"]),
            task("c", &["b"]),
        ]),
        &Limits::default(),
    )
    .unwrap_err();
    let cycle = faults
        .iter()
        .find_map(|f| match f {
            Invalid::Cycle { labels } => Some(labels.clone()),
            _ => None,
        })
        .expect("a cycle should be reported");
    assert_eq!(cycle, vec!["a", "b", "c"]);
    assert!(format!("{}", faults[0]).contains("has to go first"));
}

#[test]
fn a_dependency_that_is_not_in_the_plan_is_named() {
    let faults = validate(&plan(vec![task("b", &["a"])]), &Limits::default()).unwrap_err();
    assert!(faults.contains(&Invalid::UnknownDependency {
        label: "b".to_string(),
        missing: "a".to_string(),
    }));
    // And it is not also reported as a cycle: a missing dependency is one fault.
    assert!(!faults.iter().any(|f| matches!(f, Invalid::Cycle { .. })));
}

#[test]
fn every_fault_is_reported_at_once_not_one_per_round_trip() {
    // A planner given one fault at a time needs a model call per fault.
    let faults = validate(
        &plan(vec![
            Task {
                label: "a".into(),
                task: "".into(),
                depends_on: vec![],
                access: Access::Read,
            },
            Task {
                label: "a".into(),
                task: "do a".into(),
                depends_on: vec!["ghost".into()],
                access: Access::Read,
            },
        ]),
        &Limits::default(),
    )
    .unwrap_err();

    assert!(
        faults
            .iter()
            .any(|f| matches!(f, Invalid::EmptyTask { .. }))
    );
    assert!(
        faults
            .iter()
            .any(|f| matches!(f, Invalid::DuplicateLabel { .. }))
    );
    assert!(
        faults
            .iter()
            .any(|f| matches!(f, Invalid::UnknownDependency { .. }))
    );
    assert!(faults.len() >= 3, "got {faults:?}");
}

#[test]
fn limits_belong_to_the_host_and_say_what_would_make_the_plan_valid() {
    let many = plan((0..20).map(|i| task(&format!("t{i}"), &[])).collect());
    let faults = validate(&many, &Limits::default()).unwrap_err();
    let said = faults
        .iter()
        .map(|f| f.to_string())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(said.contains("Combine the smallest ones"), "{said}");

    // Depth, which a planner cannot feel: each link waits for the one before it.
    let deep = plan(vec![
        task("a", &[]),
        task("b", &["a"]),
        task("c", &["b"]),
        task("d", &["c"]),
        task("e", &["d"]),
    ]);
    let faults = validate(&deep, &Limits::default()).unwrap_err();
    assert!(
        faults
            .iter()
            .any(|f| matches!(f, Invalid::TooDeep { depth: 5, .. }))
    );
}

#[test]
fn a_task_depending_on_itself_is_told_so_plainly() {
    let faults = validate(&plan(vec![task("a", &["a"])]), &Limits::default()).unwrap_err();
    assert!(faults.contains(&Invalid::SelfDependency { label: "a".into() }));
}

#[test]
fn what_may_run_now_has_one_definition() {
    let valid = validate(
        &plan(vec![
            task("api", &[]),
            task("screen", &["api"]),
            task("tests", &["api"]),
        ]),
        &Limits::default(),
    )
    .unwrap();

    let nothing_done = BTreeSet::new();
    let ready: Vec<&str> = valid
        .ready(&nothing_done)
        .iter()
        .map(|t| t.label.as_str())
        .collect();
    assert_eq!(ready, vec!["api"]);

    let api_done: BTreeSet<String> = ["api".to_string()].into_iter().collect();
    let ready: Vec<&str> = valid
        .ready(&api_done)
        .iter()
        .map(|t| t.label.as_str())
        .collect();
    assert_eq!(ready, vec!["screen", "tests"], "both may run at once");
}

#[test]
fn descendants_are_what_a_revision_invalidates() {
    let valid = validate(
        &plan(vec![
            task("api", &[]),
            task("screen", &["api"]),
            task("docs", &["screen"]),
            task("unrelated", &[]),
        ]),
        &Limits::default(),
    )
    .unwrap();

    let affected = valid.descendants("api");
    assert!(affected.contains("screen") && affected.contains("docs"));
    assert!(
        !affected.contains("unrelated"),
        "a revision must not invalidate work that never read it"
    );
}

// --- inputs: the half that makes a dependency more than an ordering ---

#[test]
fn a_task_with_no_dependencies_starts_from_the_run_s_base() {
    let produced = BTreeMap::new();
    let input = inputs_for(&task("a", &[]), Some("abc123"), &produced, |_| {
        panic!("nothing to prepare")
    })
    .unwrap();
    assert_eq!(
        input,
        Input::Base {
            commit: Some("abc123".into())
        }
    );
}

#[test]
fn a_reading_task_receives_its_dependencies_output_verbatim() {
    let mut produced = BTreeMap::new();
    produced.insert(
        "api".to_string(),
        Artifact::new("api", 1, "fn get_user() -> User"),
    );

    let input = inputs_for(&task("docs", &["api"]), None, &produced, |_| {
        panic!("a reading task needs no commit")
    })
    .unwrap();

    match input {
        Input::Artifacts { from } => {
            assert_eq!(from.len(), 1);
            assert_eq!(from[0].content, "fn get_user() -> User");
        }
        other => panic!("expected artifacts, got {other:?}"),
    }
}

#[test]
fn a_writing_task_receives_a_commit_containing_its_dependencies_changes() {
    // Ordering alone would leave it editing code that does not have the API it
    // was told to build on.
    let mut produced = BTreeMap::new();
    produced.insert("api".to_string(), Artifact::new("api", 1, "added get_user"));

    let mut screen = task("screen", &["api"]);
    screen.access = Access::Write;

    let input = inputs_for(&screen, Some("base"), &produced, |merged| {
        assert_eq!(merged, ["api".to_string()]);
        Ok("merged-commit-sha".to_string())
    })
    .unwrap();

    assert_eq!(
        input,
        Input::PreparedCommit {
            commit: "merged-commit-sha".into(),
            merged: vec!["api".into()],
        }
    );
}

#[test]
fn a_conflict_is_a_finding_and_not_something_the_host_resolves() {
    let mut produced = BTreeMap::new();
    produced.insert("a".to_string(), Artifact::new("a", 1, "x"));
    produced.insert("b".to_string(), Artifact::new("b", 1, "y"));

    let mut merger = task("c", &["a", "b"]);
    merger.access = Access::Write;

    let error = inputs_for(&merger, None, &produced, |_| {
        Err("both changed src/lib.rs".to_string())
    })
    .unwrap_err();

    assert!(matches!(error, InputError::Conflict { .. }));
    let said = error.to_string();
    assert!(said.contains("does not choose a side"), "{said}");
    assert!(said.contains("finding for the verifier"), "{said}");
}

#[test]
fn starting_a_task_before_its_dependency_produced_anything_is_a_host_fault() {
    let produced = BTreeMap::new();
    let error = inputs_for(&task("b", &["a"]), None, &produced, |_| Ok("c".into())).unwrap_err();
    assert_eq!(
        error,
        InputError::NotReady {
            label: "b".into(),
            missing: "a".into()
        }
    );
}

#[test]
fn an_artifact_is_named_by_what_it_is_not_by_when_it_was_made() {
    let one = Artifact::new("api", 1, "same bytes");
    let two = Artifact::new("api", 1, "same bytes");
    assert_eq!(one.digest, two.digest);
    assert_eq!(one.name(), two.name());

    let changed = Artifact::new("api", 2, "different bytes");
    assert_ne!(one.digest, changed.digest);
}

#[test]
fn a_dependency_re_run_to_the_same_output_does_not_invalidate_its_dependants() {
    // Re-running is not the same as changing. Comparing by digest is what keeps
    // a repair from cascading through work that would read exactly the same
    // thing again.
    let mut produced = BTreeMap::new();
    produced.insert("api".to_string(), Artifact::new("api", 1, "fn get_user()"));
    let consumed = Consumed {
        label: "screen".into(),
        artifacts: vec![produced["api"].name()],
    };
    assert!(!stale(&consumed, &produced));

    // Same content, new revision: the name carries the revision, so this *is*
    // a change by the recorded rule.
    produced.insert("api".to_string(), Artifact::new("api", 2, "fn get_user()"));
    assert!(stale(&consumed, &produced));

    // Different content: unambiguously stale.
    produced.insert(
        "api".to_string(),
        Artifact::new("api", 1, "fn get_account()"),
    );
    assert!(stale(&consumed, &produced));
}

#[test]
fn acceptance_b_sees_the_exact_interface_a_defined() {
    // Codex's review, point 2: "A creates an interface; B sees and uses that
    // exact interface. Proving only that A ran before B is insufficient."
    let valid = validate(
        &plan(vec![
            task("api", &[]),
            Task {
                label: "screen".into(),
                task: "build the screen on the API".into(),
                depends_on: vec!["api".into()],
                access: Access::Read,
            },
        ]),
        &Limits::default(),
    )
    .unwrap();

    // Ordering: necessary, and on its own not enough.
    let order = valid.order();
    assert!(order.iter().position(|l| l == "api") < order.iter().position(|l| l == "screen"));

    // The part that matters: what B actually receives.
    let interface = "pub fn get_user(id: UserId) -> Result<User, Error>";
    let mut produced = BTreeMap::new();
    produced.insert("api".to_string(), Artifact::new("api", 1, interface));

    let input = inputs_for(valid.task("screen").unwrap(), None, &produced, |_| {
        panic!("reading task")
    })
    .unwrap();

    match input {
        Input::Artifacts { from } => assert_eq!(
            from[0].content, interface,
            "B must see the exact interface A defined, not a description of it"
        ),
        other => panic!("expected artifacts, got {other:?}"),
    }
}
