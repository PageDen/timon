//! The hand-off tool: giving Timon work from inside a Codex session.
//!
//! The path was qualified before it was built, because the plan rested on an
//! assumption nobody had checked — that a session can call a Timon tool with its
//! sandbox on. Against Codex 0.155.1 it can, through the app-server protocol
//! Desktop speaks, and cannot through `codex exec`, which pins approval to
//! `never`. These tests cover the tool itself; that qualification is recorded in
//! the module's own documentation because it is a fact about Codex, not about
//! this code.

use timon::mcp::handoff::{HandoffPolicy, HandoffRequest, describe, handoff};
use timon::run::record::{Runs, Status};

const NOW: i64 = 1_790_000_000;

fn policy(dir: &std::path::Path) -> HandoffPolicy {
    HandoffPolicy {
        store: dir.join("runs.sqlite"),
        workspace_root: None,
        default_max_attempts: 64,
        max_attempts_limit: 128,
        attempt_reserve: 20_000,
        principal_uid: 1000,
    }
}

fn request(goal: &str) -> HandoffRequest {
    serde_json::from_value(serde_json::json!({ "goal": goal })).unwrap()
}

#[test]
fn a_hand_off_records_a_run_and_says_how_to_find_it_later() {
    let dir = tempfile::tempdir().unwrap();
    let policy = policy(dir.path());
    let (text, error) = handoff(&policy, request("tidy the changelog"), NOW);
    assert!(!error, "{text}");

    let runs = Runs::open(&policy.store).unwrap();
    let recorded = runs.recent(10).unwrap();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].goal, "tidy the changelog");
    assert_eq!(recorded[0].principal_uid, 1000);
    assert_eq!(recorded[0].status, Status::Running);

    // The session is told the id, because a run it cannot find again is a run
    // the developer has lost.
    assert!(text.contains(&recorded[0].id));
    assert!(text.contains("timon runs show"));
    assert!(text.contains("timon runs cancel"));
}

#[test]
fn the_session_is_told_plainly_that_nothing_was_spent_yet() {
    // Without this the model will tell the developer their work has started.
    let dir = tempfile::tempdir().unwrap();
    let (text, _) = handoff(&policy(dir.path()), request("something"), NOW);
    assert!(text.contains("no quota has been spent"), "was: {text}");
    assert!(text.contains("Nothing has been written to the working files"));
}

#[test]
fn a_session_cannot_raise_the_attempt_limit_past_the_server_s() {
    // A limit a session can widen is not a limit. The server is started by the
    // developer; the model only gets to ask for less.
    let dir = tempfile::tempdir().unwrap();
    let policy = policy(dir.path());
    let greedy: HandoffRequest = serde_json::from_value(serde_json::json!({
        "goal": "big job",
        "max_attempts": 100_000
    }))
    .unwrap();
    let (text, error) = handoff(&policy, greedy, NOW);
    assert!(!error, "{text}");

    let runs = Runs::open(&policy.store).unwrap();
    let recorded = &runs.recent(1).unwrap()[0];
    assert_eq!(recorded.max_attempts, 128, "capped at the server's limit");

    // Asking for fewer is honoured, because that direction is safe.
    let modest: HandoffRequest =
        serde_json::from_value(serde_json::json!({ "goal": "small job", "max_attempts": 4 }))
            .unwrap();
    handoff(&policy, modest, NOW + 1);
    let recorded = &runs.recent(1).unwrap()[0];
    assert_eq!(recorded.max_attempts, 4);
}

#[test]
fn a_refusal_comes_back_as_a_tool_result_the_model_can_relay() {
    // Not a protocol error. The developer should read why Timon declined, not
    // "the tool broke".
    let dir = tempfile::tempdir().unwrap();
    let policy = policy(dir.path());
    let hopeless: HandoffRequest = serde_json::from_value(serde_json::json!({
        "goal": "job",
        "token_ceiling": 10
    }))
    .unwrap();
    let (text, error) = handoff(&policy, hopeless, NOW);
    assert!(error);
    assert!(text.contains("did not start the run"));
    assert!(
        text.contains("cannot admit even one attempt"),
        "was: {text}"
    );

    let runs = Runs::open(&policy.store).unwrap();
    assert!(runs.recent(10).unwrap().is_empty());
}

#[test]
fn an_empty_goal_is_refused_before_anything_is_opened() {
    let dir = tempfile::tempdir().unwrap();
    let (text, error) = handoff(&policy(dir.path()), request("   "), NOW);
    assert!(error);
    assert!(text.contains("goal was empty"));
}

#[test]
fn resubmitting_with_the_same_key_returns_the_same_run() {
    // A session that lost its reply and asked again has made one request.
    let dir = tempfile::tempdir().unwrap();
    let policy = policy(dir.path());
    let make = || -> HandoffRequest {
        serde_json::from_value(serde_json::json!({
            "goal": "idempotent",
            "submission_key": "key-1"
        }))
        .unwrap()
    };
    let (first, _) = handoff(&policy, make(), NOW);
    let (second, _) = handoff(&policy, make(), NOW + 30);

    let runs = Runs::open(&policy.store).unwrap();
    assert_eq!(runs.recent(10).unwrap().len(), 1, "one run, not two");

    let id = &runs.recent(1).unwrap()[0].id;
    assert!(first.contains(id) && second.contains(id));
}

#[test]
fn the_tool_description_tells_the_model_what_it_is_for_and_what_it_is_not() {
    let tool = describe();
    assert_eq!(tool["name"], "timon_handoff");
    let description = tool["description"].as_str().unwrap();
    // A model chooses tools from this text, so it has to say when to reach for
    // it and what it will not do.
    assert!(description.contains("outlive this conversation"));
    assert!(description.contains("branch and a report"));
    assert!(description.contains("Nothing is written to the user's working files"));
    assert_eq!(tool["inputSchema"]["required"][0], "goal");
}

#[test]
fn a_hand_off_carries_account_names_and_never_a_credential() {
    let dir = tempfile::tempdir().unwrap();
    let policy = policy(dir.path());
    let scoped: HandoffRequest = serde_json::from_value(serde_json::json!({
        "goal": "job",
        "accounts": ["acct2"]
    }))
    .unwrap();
    handoff(&policy, scoped, NOW);

    let runs = Runs::open(&policy.store).unwrap();
    let recorded = &runs.recent(1).unwrap()[0];
    assert_eq!(recorded.accounts, vec!["acct2".to_string()]);
    let rendered = serde_json::to_string(recorded).unwrap();
    assert!(!rendered.contains("Bearer") && !rendered.contains("access_token"));
}

#[test]
fn a_session_cannot_redirect_a_hand_off_outside_the_server_s_repository() {
    // Found by testing, not by review: a live Codex session called the tool and
    // quietly passed its own working directory, so the run was recorded against
    // a repository the developer had not chosen. A model choosing the workspace
    // is a model choosing what the run reads now and writes later.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    let inside = root.join("crate-a");
    let outside = dir.path().join("elsewhere");
    std::fs::create_dir_all(&inside).unwrap();
    std::fs::create_dir_all(&outside).unwrap();

    let mut policy = policy(dir.path());
    policy.workspace_root = Some(root.clone());

    let elsewhere: HandoffRequest = serde_json::from_value(serde_json::json!({
        "goal": "job",
        "workspace": outside.to_string_lossy()
    }))
    .unwrap();
    let (text, error) = handoff(&policy, elsewhere, NOW);
    assert!(error, "was: {text}");
    assert!(text.contains("outside it"), "was: {text}");

    let runs = Runs::open(&policy.store).unwrap();
    assert!(runs.recent(10).unwrap().is_empty(), "nothing recorded");

    // Narrowing to a path inside the root is allowed: it takes nothing away.
    let narrowed: HandoffRequest = serde_json::from_value(serde_json::json!({
        "goal": "job",
        "workspace": inside.to_string_lossy()
    }))
    .unwrap();
    let (_, error) = handoff(&policy, narrowed, NOW + 1);
    assert!(!error);
    assert_eq!(
        runs.recent(1).unwrap()[0].workspace,
        Some(inside.canonicalize().unwrap())
    );
}

#[test]
fn omitting_the_workspace_uses_the_server_s_own() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    let mut policy = policy(dir.path());
    policy.workspace_root = Some(root.clone());

    handoff(&policy, request("job"), NOW);
    let runs = Runs::open(&policy.store).unwrap();
    assert_eq!(runs.recent(1).unwrap()[0].workspace, Some(root));
}
