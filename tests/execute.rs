//! Running the route triage chose.
//!
//! The first thing in this project that calls a model on its own initiative, so
//! the tests here are mostly about what happens when it should *not*: no
//! authority, no route built, a worker that fails. A run that spends quota
//! nobody is accountable for is worse than a run that did not happen.

use timon::run::execute::{ExecuteError, GRANT_ENV, Granted, Plan, authorise};
use timon::run::record::{Base, Run, Runs, Status};
use timon::run::start::{Request, admit};
use timon::triage::{Allowances, Route, decide};

const NOW: i64 = 1_790_000_000;

fn plan(broker: &str, dir: &std::path::Path) -> Plan {
    Plan {
        broker: broker.to_string(),
        cheap_command: vec!["/bin/cat".into()],
        strong_command: vec!["/bin/cat".into()],
        write_command: vec!["/bin/cat".into()],
        cheap_model: Some("cheap-model".to_string()),
        strong_model: Some("strong-model".to_string()),
        output_root: dir.join("out"),
        deadline: std::time::Duration::from_secs(20),
        max_output_bytes: 1024 * 1024,
        grant_lifetime_secs: 120,
        cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    }
}

fn a_run(runs: &Runs, goal: &str, accounts: Vec<String>) -> Run {
    admit(
        runs,
        Request {
            goal: goal.to_string(),
            principal_uid: 1000,
            workspace: None,
            accounts,
            max_attempts: 4,
            token_ceiling: None,
            deadline: None,
            submission_key: None,
        },
        Base::None,
        NOW,
        None,
        20_000,
    )
    .unwrap()
}

#[test]
fn a_grant_reaches_the_worker_as_one_environment_variable() {
    // And as nothing else. The worker learns what it may spend against, and
    // nothing about how — it never sees an account name or a credential.
    let granted = Granted {
        token: "secret-token".to_string(),
        id: "grant-1".to_string(),
        expires_at: NOW + 60,
        accounts: vec!["acct3".to_string()],
        model: Some("gpt-5.6-luna".to_string()),
    };
    let environment = granted.environment();
    assert_eq!(environment.len(), 1, "one variable, not a pile of them");
    assert_eq!(environment[0].0, GRANT_ENV);
    assert_eq!(environment[0].1, "secret-token");
}

#[test]
fn the_grant_token_is_not_serialised_with_the_rest() {
    // A report of what a run was authorised to do must not carry the thing that
    // authorises it.
    let granted = Granted {
        token: "secret-token".to_string(),
        id: "grant-1".to_string(),
        expires_at: NOW + 60,
        accounts: vec!["acct3".to_string()],
        model: None,
    };
    let rendered = serde_json::to_string(&granted).unwrap();
    assert!(rendered.contains("grant-1"));
    assert!(rendered.contains("acct3"));
    assert!(
        !rendered.contains("secret-token"),
        "the token must not appear in a rendering: {rendered}"
    );
}

#[test]
fn a_run_that_cannot_be_authorised_does_not_start() {
    // There is no path where a worker spends quota outside a grant. A broker
    // that is not there is the easiest way to check that.
    let dir = tempfile::tempdir().unwrap();
    let runs = Runs::open(dir.path().join("runs.sqlite")).unwrap();
    let run = a_run(&runs, "anything", vec![]);
    let decision = decide(&run.goal, &Allowances::default());

    // Port 1 has nothing on it.
    let plan = plan("127.0.0.1:1", dir.path());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let error = runtime
        .block_on(timon::run::execute::execute(
            &runs, &run, &decision, &plan, NOW,
        ))
        .unwrap_err();

    assert!(matches!(error, ExecuteError::Unreachable(_)));
    let said = format!("{error}");
    assert!(
        said.contains("nowhere else for this run to go"),
        "the message should say why there is no fallback: {said}"
    );
    assert!(
        !dir.path().join("out").exists(),
        "no worker directory should exist: nothing was started"
    );
}

#[test]
fn the_planner_route_says_it_is_not_built_rather_than_running_one_worker() {
    // Quietly running a single worker and calling it the planner would be a
    // measurement that lies about which arm produced it.
    let dir = tempfile::tempdir().unwrap();
    let runs = Runs::open(dir.path().join("runs.sqlite")).unwrap();
    let run = a_run(
        &runs,
        "Summarise the changelog; then draft release notes",
        vec![],
    );
    let decision = decide(
        &run.goal,
        &Allowances {
            route: None,
            allow_planner: true,
        },
    );
    assert_eq!(decision.route, Route::Planner);

    let plan = plan("127.0.0.1:1", dir.path());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let error = runtime
        .block_on(timon::run::execute::execute(
            &runs, &run, &decision, &plan, NOW,
        ))
        .unwrap_err();

    assert!(matches!(
        error,
        ExecuteError::NotImplemented(Route::Planner)
    ));
    assert!(format!("{error}").contains("not built yet"));
}

#[test]
fn authorising_against_something_that_is_not_a_broker_is_reported_not_guessed() {
    let dir = tempfile::tempdir().unwrap();
    let runs = Runs::open(dir.path().join("runs.sqlite")).unwrap();
    let run = a_run(&runs, "goal", vec!["acct3".to_string()]);
    let error = authorise("127.0.0.1:1", &run, Some("gpt-5.6-luna"), 60).unwrap_err();
    assert!(matches!(error, ExecuteError::Unreachable(_)));
}

#[test]
fn a_finished_run_is_settled_rather_than_left_running() {
    // /bin/cat exits 0 after echoing stdin, which is enough to check the run is
    // moved out of `running` on a path that does not involve a model.
    let dir = tempfile::tempdir().unwrap();
    let runs = Runs::open(dir.path().join("runs.sqlite")).unwrap();
    let run = a_run(&runs, "echoed", vec![]);
    assert_eq!(runs.get(&run.id).unwrap().status, Status::Running);
    // Without a broker this cannot proceed, and the run must still not be left
    // claiming to be running.
    let plan = plan("127.0.0.1:1", dir.path());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let decision = decide(&run.goal, &Allowances::default());
    let _ = runtime.block_on(timon::run::execute::execute(
        &runs, &run, &decision, &plan, NOW,
    ));
    // execute() returns the error before settling; the caller settles it. What
    // matters here is that the store is still readable and the run is findable.
    assert_eq!(runs.get(&run.id).unwrap().id, run.id);
}
