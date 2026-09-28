//! Running a validated graph.
//!
//! Three behaviours carry the weight, because the obvious implementation gets
//! each of them wrong: a blocked task is reported as never run rather than
//! failed, cancellation reaches the workers rather than just the loop, and the
//! deadline stops new work instead of starting work it will immediately kill.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use timon::dag::{Access, Limits, Plan, Task, Validated, validate};
use timon::dag_inputs::{Artifact, Input};
use timon::dag_run::{Bounds, Outcome, Runner, run_graph};

fn task(label: &str, depends_on: &[&str]) -> Task {
    Task {
        label: label.to_string(),
        task: format!("do {label}"),
        depends_on: depends_on.iter().map(|s| s.to_string()).collect(),
        access: Access::Read,
    }
}

fn graph(tasks: Vec<Task>) -> Validated {
    validate(
        &Plan {
            tasks,
            notes: String::new(),
        },
        &Limits::default(),
    )
    .expect("valid")
}

/// A runner that answers from a script, so the scheduler's decisions are what
/// is under test rather than anything a model did.
struct Scripted {
    /// Labels that should fail, with the reason.
    fail: Vec<(&'static str, &'static str)>,
    /// Labels seen, in the order they started.
    seen: std::sync::Mutex<Vec<String>>,
    running: AtomicUsize,
    peak: AtomicUsize,
    now: std::sync::Mutex<i64>,
    /// Set by a task to cancel the run from inside it.
    cancel_on: Option<&'static str>,
    cancel_flag: std::sync::Mutex<Option<Arc<AtomicBool>>>,
    /// Task that should notice cancellation and stop.
    observed_cancel: AtomicBool,
}

impl Scripted {
    fn new() -> Self {
        Scripted {
            fail: Vec::new(),
            seen: std::sync::Mutex::new(Vec::new()),
            running: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            now: std::sync::Mutex::new(1_790_000_000),
            cancel_on: None,
            cancel_flag: std::sync::Mutex::new(None),
            observed_cancel: AtomicBool::new(false),
        }
    }
}

impl Runner for Scripted {
    fn run(&self, task: &Task, _input: &Input, cancel: &AtomicBool) -> Result<Artifact, String> {
        let live = self.running.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(live, Ordering::SeqCst);
        self.seen.lock().unwrap().push(task.label.clone());
        std::thread::sleep(std::time::Duration::from_millis(20));

        if cancel.load(Ordering::Relaxed) {
            self.observed_cancel.store(true, Ordering::SeqCst);
            self.running.fetch_sub(1, Ordering::SeqCst);
            return Err("cancelled".to_string());
        }
        if Some(task.label.as_str()) == self.cancel_on
            && let Some(flag) = self.cancel_flag.lock().unwrap().as_ref()
        {
            flag.store(true, Ordering::SeqCst);
        }

        self.running.fetch_sub(1, Ordering::SeqCst);
        if let Some((_, why)) = self.fail.iter().find(|(l, _)| *l == task.label) {
            return Err((*why).to_string());
        }
        Ok(Artifact::new(
            &task.label,
            1,
            format!("output of {}", task.label),
        ))
    }

    fn now(&self) -> i64 {
        *self.now.lock().unwrap()
    }
}

#[test]
fn a_graph_runs_in_dependency_order_and_reports_every_task() {
    let plan = graph(vec![
        task("api", &[]),
        task("screen", &["api"]),
        task("docs", &["screen"]),
    ]);
    let runner = Scripted::new();
    let report = run_graph(
        &plan,
        &runner,
        &Bounds::default(),
        Arc::new(AtomicBool::new(false)),
    );

    assert!(report.complete);
    assert_eq!(report.tasks.len(), 3);
    assert_eq!(*runner.seen.lock().unwrap(), vec!["api", "screen", "docs"]);
    assert_eq!(report.artifact("api").unwrap().content, "output of api");
}

#[test]
fn independent_tasks_run_at_once_up_to_the_limit() {
    let plan = graph(vec![
        task("a", &[]),
        task("b", &[]),
        task("c", &[]),
        task("d", &[]),
    ]);
    let runner = Scripted::new();
    let bounds = Bounds {
        concurrency: 2,
        deadline: None,
        writing_permitted: false,
    };
    let report = run_graph(&plan, &runner, &bounds, Arc::new(AtomicBool::new(false)));

    assert!(report.complete);
    assert_eq!(
        runner.peak.load(Ordering::SeqCst),
        2,
        "the limit is a limit, and it is also actually used"
    );
}

#[test]
fn a_blocked_task_is_reported_as_never_run_not_as_failed() {
    // The distinction the whole report hangs on: nobody should spend an
    // afternoon debugging a task that never started.
    let plan = graph(vec![
        task("api", &[]),
        task("screen", &["api"]),
        task("docs", &["screen"]),
        task("unrelated", &[]),
    ]);
    let mut runner = Scripted::new();
    runner.fail = vec![("api", "the provider refused")];

    let report = run_graph(
        &plan,
        &runner,
        &Bounds::default(),
        Arc::new(AtomicBool::new(false)),
    );

    assert!(!report.complete);
    let outcome = |label: &str| {
        report
            .tasks
            .iter()
            .find(|t| t.label == label)
            .map(|t| t.outcome.clone())
            .unwrap()
    };

    assert_eq!(
        outcome("api"),
        Outcome::Failed {
            detail: "the provider refused".into()
        }
    );
    assert_eq!(
        outcome("screen"),
        Outcome::NotRun {
            blocked_by: vec!["api".into()]
        },
        "and it names what blocked it"
    );
    assert!(matches!(outcome("docs"), Outcome::NotRun { .. }));
    assert!(
        outcome("unrelated").finished(),
        "work that did not depend on the failure still runs"
    );
    assert_eq!(report.not_run, vec!["screen", "docs"]);
}

#[test]
fn cancellation_reaches_the_workers_and_not_only_the_loop() {
    // Stopping the scheduler while children keep talking to a provider is not
    // cancelling, it is losing track. This is the gap P2's executor left.
    let plan = graph(vec![
        task("first", &[]),
        task("second", &["first"]),
        task("third", &["second"]),
    ]);
    let cancel = Arc::new(AtomicBool::new(false));
    let mut runner = Scripted::new();
    runner.cancel_on = Some("first");
    *runner.cancel_flag.lock().unwrap() = Some(Arc::clone(&cancel));

    let report = run_graph(&plan, &runner, &Bounds::default(), cancel);

    let outcome = |label: &str| {
        report
            .tasks
            .iter()
            .find(|t| t.label == label)
            .map(|t| t.outcome.clone())
            .unwrap()
    };
    assert!(outcome("first").finished(), "it finished before cancelling");
    assert_eq!(outcome("second"), Outcome::Cancelled);
    assert_eq!(
        outcome("third"),
        Outcome::Cancelled,
        "not NotRun: the run was stopped, its dependency did not fail"
    );
}

#[test]
fn a_worker_that_notices_cancellation_mid_flight_is_recorded_as_cancelled() {
    let plan = graph(vec![task("a", &[]), task("b", &[])]);
    let cancel = Arc::new(AtomicBool::new(true));
    let runner = Scripted::new();
    let report = run_graph(&plan, &runner, &Bounds::default(), cancel);

    for report in &report.tasks {
        assert_eq!(report.outcome, Outcome::Cancelled);
    }
    assert!(!report.complete);
}

#[test]
fn the_deadline_stops_new_work_rather_than_starting_work_to_kill_it() {
    // Starting a task at the deadline so it can be killed a second later spends
    // tokens for nothing.
    let plan = graph(vec![task("a", &[]), task("b", &["a"])]);
    let runner = Scripted::new();
    let bounds = Bounds {
        concurrency: 2,
        deadline: Some(1_789_999_999), // already passed
        writing_permitted: false,
    };
    let report = run_graph(&plan, &runner, &bounds, Arc::new(AtomicBool::new(false)));

    assert!(
        runner.seen.lock().unwrap().is_empty(),
        "nothing was started"
    );
    assert_eq!(report.tasks[0].outcome, Outcome::Skipped);
}

#[test]
fn a_writing_task_whose_dependencies_conflict_fails_with_the_hosts_reason() {
    // The host merges and does not choose a side, so the conflict is reported
    // rather than resolved.
    struct Conflicting(Scripted);
    impl Runner for Conflicting {
        fn run(&self, task: &Task, input: &Input, cancel: &AtomicBool) -> Result<Artifact, String> {
            self.0.run(task, input, cancel)
        }
        fn prepare(&self, _merged: &[String]) -> Result<String, String> {
            Err("both changed src/lib.rs".to_string())
        }
        fn now(&self) -> i64 {
            self.0.now()
        }
    }

    let mut writer = task("merge", &["a", "b"]);
    writer.access = Access::Write;
    let plan = graph(vec![task("a", &[]), task("b", &[]), writer]);
    let runner = Conflicting(Scripted::new());

    let report = run_graph(
        &plan,
        &runner,
        &Bounds {
            concurrency: 3,
            deadline: None,
            // Qualified, so this test reaches input preparation rather than
            // stopping at the gate.
            writing_permitted: true,
        },
        Arc::new(AtomicBool::new(false)),
    );

    let merge = report.tasks.iter().find(|t| t.label == "merge").unwrap();
    match &merge.outcome {
        Outcome::Failed { detail } => {
            assert!(detail.contains("both changed src/lib.rs"), "{detail}");
            assert!(detail.contains("does not choose a side"), "{detail}");
        }
        other => panic!("expected a failure carrying the reason, got {other:?}"),
    }
}

#[test]
fn a_report_reads_the_same_way_for_the_same_plan() {
    let build = || {
        let plan = graph(vec![task("c", &[]), task("a", &[]), task("b", &["a"])]);
        let runner = Scripted::new();
        run_graph(
            &plan,
            &runner,
            &Bounds::default(),
            Arc::new(AtomicBool::new(false)),
        )
        .tasks
        .iter()
        .map(|t| t.label.clone())
        .collect::<Vec<_>>()
    };
    let first = build();
    for _ in 0..10 {
        assert_eq!(build(), first);
    }
    assert_eq!(first, vec!["a", "c", "b"]);
}

#[test]
fn a_dependants_input_carries_what_its_dependency_actually_produced() {
    // The P3 rule, exercised through the scheduler rather than in isolation.
    let plan = graph(vec![task("api", &[]), task("screen", &["api"])]);
    let runner = Scripted::new();
    let report = run_graph(
        &plan,
        &runner,
        &Bounds::default(),
        Arc::new(AtomicBool::new(false)),
    );

    let screen = report.tasks.iter().find(|t| t.label == "screen").unwrap();
    match screen.input.as_ref().expect("an input was recorded") {
        Input::Artifacts { from } => {
            assert_eq!(from.len(), 1);
            assert_eq!(from[0].content, "output of api");
        }
        other => panic!("expected artifacts, got {other:?}"),
    }
}

#[test]
fn a_writing_task_does_not_run_on_a_host_that_has_not_qualified() {
    // P4.3 gates P4.2. The scheduler asks every time rather than trusting that
    // whoever built the plan remembered — a flag somebody can forget to check
    // is not a gate.
    let mut writer = task("edit", &[]);
    writer.access = Access::Write;
    let plan = graph(vec![writer, task("read", &[])]);
    let runner = Scripted::new();

    let report = run_graph(
        &plan,
        &runner,
        &Bounds::default(), // writing_permitted defaults to false
        Arc::new(AtomicBool::new(false)),
    );

    let edit = report.tasks.iter().find(|t| t.label == "edit").unwrap();
    match &edit.outcome {
        Outcome::Failed { detail } => {
            assert!(detail.contains("has not qualified"), "{detail}");
            assert!(detail.contains("timon qualify write-sandbox"), "{detail}");
        }
        other => panic!("a writing task must not run unqualified, got {other:?}"),
    }
    assert!(
        !runner.seen.lock().unwrap().contains(&"edit".to_string()),
        "and it must not have been started at all"
    );

    // Reading work is unaffected: the gate is about writing.
    assert!(
        report
            .tasks
            .iter()
            .find(|t| t.label == "read")
            .unwrap()
            .outcome
            .finished()
    );
}

#[test]
fn a_qualified_host_runs_writing_tasks() {
    let mut writer = task("edit", &[]);
    writer.access = Access::Write;
    let plan = graph(vec![writer]);
    let runner = Scripted::new();

    let report = run_graph(
        &plan,
        &runner,
        &Bounds {
            concurrency: 1,
            deadline: None,
            writing_permitted: true,
        },
        Arc::new(AtomicBool::new(false)),
    );
    // A writing task with no dependencies needs no prepared commit, so on a
    // qualified host it simply runs.
    assert!(
        report.tasks[0].outcome.finished(),
        "got {:?}",
        report.tasks[0].outcome
    );
    assert!(runner.seen.lock().unwrap().contains(&"edit".to_string()));
}
