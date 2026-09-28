//! The run record: a hand-off that outlives the terminal that started it.
//!
//! Two properties carry the weight here, both from Codex's review. A restart
//! must neither lose a run nor duplicate one. And the state a run started from
//! must be recorded rather than assumed, because a repository with uncommitted
//! work is the normal case and "the base commit" does not say what was in it.

use timon::run::record::{Base, RunError, Runs, Status};
use timon::run::start::{Refused, Request, admit};

const ALICE: u32 = 1000;
const BOB: u32 = 1001;
const NOW: i64 = 1_790_000_000;
const RESERVE: u64 = 20_000;

fn store() -> (tempfile::TempDir, Runs) {
    let dir = tempfile::tempdir().unwrap();
    let runs = Runs::open(dir.path().join("runs.sqlite")).unwrap();
    (dir, runs)
}

fn request(goal: &str) -> Request {
    Request {
        goal: goal.to_string(),
        principal_uid: ALICE,
        workspace: None,
        accounts: Vec::new(),
        max_attempts: 8,
        token_ceiling: None,
        deadline: None,
        submission_key: None,
    }
}

#[test]
fn a_run_survives_being_written_and_read_back() {
    let (_dir, runs) = store();
    let run = admit(
        &runs,
        request("summarise the changelog"),
        Base::Head {
            commit: "a".repeat(40),
        },
        NOW,
        None,
        RESERVE,
    )
    .unwrap();

    let read = runs.get(&run.id).unwrap();
    assert_eq!(read.id, run.id);
    assert_eq!(read.goal, "summarise the changelog");
    assert_eq!(read.principal_uid, ALICE);
    assert_eq!(read.status, Status::Running);
    assert_eq!(read.base.commit(), Some("a".repeat(40).as_str()));
}

#[test]
fn the_same_submission_key_returns_the_same_run_rather_than_starting_another() {
    // A hand-off resubmitted because the first reply was lost is one piece of
    // work. Starting a second would spend twice for one request.
    let (_dir, runs) = store();
    let mut first = request("migrate the schema");
    first.submission_key = Some("idem-1".to_string());
    let one = admit(&runs, first, Base::None, NOW, None, RESERVE).unwrap();

    let mut again = request("migrate the schema");
    again.submission_key = Some("idem-1".to_string());
    let two = admit(&runs, again, Base::None, NOW + 5, None, RESERVE).unwrap();

    assert_eq!(one.id, two.id, "the same key must not start a second run");
    assert_eq!(runs.recent(10).unwrap().len(), 1);
}

#[test]
fn a_restart_marks_what_it_left_behind_instead_of_lying_about_it() {
    // The store cannot still be running a run whose process is gone. Leaving it
    // `running` would make every later status display wrong, and silently
    // deleting it would lose the fact that somebody's work stopped.
    let (_dir, runs) = store();
    let alive = admit(&runs, request("long job"), Base::None, NOW, None, RESERVE).unwrap();
    let done = admit(&runs, request("short job"), Base::None, NOW, None, RESERVE).unwrap();
    runs.settle(&done.id, Status::Finished, NOW + 10, None)
        .unwrap();

    let stranded = runs.interrupt_stale(NOW + 100).unwrap();

    assert_eq!(stranded.len(), 1, "only the live run was stranded");
    assert_eq!(stranded[0].id, alive.id);
    assert_eq!(runs.get(&alive.id).unwrap().status, Status::Interrupted);
    assert_eq!(
        runs.get(&done.id).unwrap().status,
        Status::Finished,
        "a finished run is not disturbed by a restart"
    );
    let note = runs.get(&alive.id).unwrap().detail.unwrap();
    assert!(note.contains("orchestrator stopped"), "note was: {note}");
}

#[test]
fn recovering_twice_is_not_an_error_and_strands_nothing_new() {
    let (_dir, runs) = store();
    admit(&runs, request("job"), Base::None, NOW, None, RESERVE).unwrap();
    assert_eq!(runs.interrupt_stale(NOW + 1).unwrap().len(), 1);
    assert_eq!(
        runs.interrupt_stale(NOW + 2).unwrap().len(),
        0,
        "a second recovery pass has nothing left to do"
    );
}

#[test]
fn only_the_principal_who_started_a_run_can_cancel_it() {
    // On a shared host, "who asked" is the whole basis of ownership, and the uid
    // comes from the kernel rather than from a flag.
    let (_dir, runs) = store();
    let run = admit(
        &runs,
        request("alice's job"),
        Base::None,
        NOW,
        None,
        RESERVE,
    )
    .unwrap();

    let refused = runs.cancel(&run.id, BOB, NOW + 1).unwrap_err();
    assert!(matches!(refused, RunError::NotYours { .. }));
    assert_eq!(runs.get(&run.id).unwrap().status, Status::Running);

    let cancelled = runs.cancel(&run.id, ALICE, NOW + 2).unwrap();
    assert_eq!(cancelled.status, Status::Cancelling);
}

#[test]
fn cancelling_a_finished_run_is_not_an_error() {
    let (_dir, runs) = store();
    let run = admit(&runs, request("job"), Base::None, NOW, None, RESERVE).unwrap();
    runs.settle(&run.id, Status::Finished, NOW + 5, None)
        .unwrap();
    let after = runs.cancel(&run.id, ALICE, NOW + 6).unwrap();
    assert_eq!(
        after.status,
        Status::Finished,
        "cancelling something already done leaves it done"
    );
}

#[test]
fn a_deadline_already_passed_is_refused_before_anything_is_spent() {
    let (_dir, runs) = store();
    let mut late = request("job");
    late.deadline = Some(NOW - 1);
    let refused = admit(&runs, late, Base::None, NOW, None, RESERVE).unwrap_err();
    assert!(matches!(refused, Refused::DeadlinePassed { .. }));
    assert!(
        runs.recent(10).unwrap().is_empty(),
        "a refused run is not recorded as started"
    );
}

#[test]
fn a_ceiling_too_small_for_one_attempt_is_refused_at_submission() {
    // Better than admitting it and refusing its first task, which spends the
    // caller's time to reach the same answer.
    let (_dir, runs) = store();
    let mut tiny = request("job");
    tiny.token_ceiling = Some(RESERVE - 1);
    let refused = admit(&runs, tiny, Base::None, NOW, None, RESERVE).unwrap_err();
    assert!(matches!(refused, Refused::CeilingBelowOneAttempt { .. }));
    assert!(format!("{refused}").contains("cannot admit even one attempt"));
}

#[test]
fn no_free_slot_refuses_the_run_and_says_where_to_look() {
    let (_dir, runs) = store();
    let refused = admit(
        &runs,
        request("job"),
        Base::None,
        NOW,
        Some((4, 4)),
        RESERVE,
    )
    .unwrap_err();
    assert!(matches!(refused, Refused::NoSlot { limit: 4 }));
    assert!(format!("{refused}").contains("slots report"));
}

#[test]
fn a_workspace_that_is_not_a_directory_is_refused() {
    let (dir, runs) = store();
    let file = dir.path().join("a-file");
    std::fs::write(&file, b"not a directory").unwrap();
    let mut bad = request("job");
    bad.workspace = Some(file);
    let refused = admit(&runs, bad, Base::None, NOW, None, RESERVE).unwrap_err();
    assert!(matches!(refused, Refused::Workspace { .. }));
}

#[test]
fn a_dirty_working_tree_is_recorded_as_a_snapshot_not_as_clean() {
    // The distinction a reader needs: whether uncommitted work was part of what
    // ran. Saying "the base commit" alone hides it.
    let clean = Base::Head {
        commit: "c".repeat(40),
    };
    assert!(clean.describe().contains("clean"));

    let dirty = Base::Snapshot {
        commit: "c".repeat(40),
        snapshot: "d".repeat(40),
    };
    let described = dirty.describe();
    assert!(described.contains("uncommitted"), "was: {described}");
    assert!(described.contains(&"d".repeat(12)), "names the snapshot");
    assert_eq!(dirty.commit(), Some("c".repeat(40).as_str()));

    assert_eq!(Base::None.commit(), None);
    assert!(Base::None.describe().contains("not a repository"));
}

#[test]
fn a_run_knows_when_its_deadline_has_passed() {
    let (_dir, runs) = store();
    let mut bounded = request("job");
    bounded.deadline = Some(NOW + 60);
    let run = admit(&runs, bounded, Base::None, NOW, None, RESERVE).unwrap();
    assert!(!run.overdue(NOW + 59));
    assert!(run.overdue(NOW + 60));
}

#[test]
fn an_unknown_run_is_reported_by_name_rather_than_as_a_crash() {
    let (_dir, runs) = store();
    let error = runs.get("run-does-not-exist").unwrap_err();
    assert!(matches!(error, RunError::Unknown(_)));
    assert!(format!("{error}").contains("run-does-not-exist"));
}

#[test]
fn a_run_record_carries_no_credential_and_names_its_accounts_only() {
    // The run says which pooled accounts it may spend. It never holds a token,
    // and the broker remains the only thing that can.
    let (_dir, runs) = store();
    let mut scoped = request("job");
    scoped.accounts = vec!["acct2".to_string()];
    let run = admit(&runs, scoped, Base::None, NOW, None, RESERVE).unwrap();
    let rendered = serde_json::to_string(&run).unwrap();
    assert!(rendered.contains("acct2"));
    assert!(!rendered.contains("Bearer"));
    assert!(!rendered.contains("access_token"));
}

#[test]
fn runs_are_listed_newest_first() {
    let (_dir, runs) = store();
    let first = admit(&runs, request("first"), Base::None, NOW, None, RESERVE).unwrap();
    let second = admit(
        &runs,
        request("second"),
        Base::None,
        NOW + 10,
        None,
        RESERVE,
    )
    .unwrap();
    let listed: Vec<String> = runs
        .recent(10)
        .unwrap()
        .into_iter()
        .map(|run| run.id)
        .collect();
    assert_eq!(listed, vec![second.id, first.id]);
}

#[test]
fn a_status_round_trips_through_its_text_form() {
    for status in [
        Status::Running,
        Status::Finished,
        Status::Cancelling,
        Status::Cancelled,
        Status::Interrupted,
    ] {
        assert_eq!(Status::parse(status.as_str()), Some(status));
    }
    assert_eq!(Status::parse("nonsense"), None);
    assert!(Status::Running.live() && Status::Cancelling.live());
    assert!(!Status::Finished.live() && !Status::Interrupted.live());
}
