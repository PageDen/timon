//! Cancellation reaching a worker that another process started.
//!
//! Ctrl-C always worked: the executor watches a flag in its own process. A
//! `timon runs cancel` from a second terminal did not — it wrote `Cancelling`
//! to the store and revoked the grant, so nothing new was authorised, but the
//! turn already running carried on in a process that never read the record.
//!
//! The property under test is that the record is now the channel between the
//! two processes, and the safety property is that failing to read it never
//! cancels anything.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use timon::run::record::{Base, Runs, Status};
use timon::run::start::{Request, admit};
use timon::run::watch::{Look, look, watch};

const ALICE: u32 = 1000;
const NOW: i64 = 1_790_000_000;
const RESERVE: u64 = 20_000;

fn started(dir: &std::path::Path, goal: &str) -> (Runs, String) {
    let runs = Runs::open(dir.join("runs.sqlite")).unwrap();
    let run = admit(
        &runs,
        Request {
            goal: goal.to_string(),
            principal_uid: ALICE,
            workspace: None,
            accounts: Vec::new(),
            max_attempts: 8,
            token_ceiling: None,
            deadline: None,
            submission_key: None,
        },
        Base::Head {
            commit: "a".repeat(40),
        },
        NOW,
        None,
        RESERVE,
    )
    .unwrap();
    let id = run.id.clone();
    (runs, id)
}

#[test]
fn a_running_record_says_carry_on() {
    let dir = tempfile::tempdir().unwrap();
    let (runs, id) = started(dir.path(), "a goal");
    assert_eq!(look(&runs, &id), Look::Continue);
}

#[test]
fn a_cancelling_record_says_cancel() {
    let dir = tempfile::tempdir().unwrap();
    let (runs, id) = started(dir.path(), "a goal");
    runs.cancel(&id, ALICE, NOW + 10).unwrap();
    assert_eq!(look(&runs, &id), Look::Cancel);
}

#[test]
fn a_finished_record_ends_the_watch_without_cancelling() {
    let dir = tempfile::tempdir().unwrap();
    let (runs, id) = started(dir.path(), "a goal");
    runs.settle(&id, Status::Finished, NOW + 20, None).unwrap();
    assert_eq!(look(&runs, &id), Look::Finished);
}

/// The safety property, and the reason `look` returns four things rather than a
/// bool. A watcher that treated an unreadable record as a cancellation would
/// turn brief lock contention, or a store moved underneath a run, into a killed
/// worker — a worse failure than the one this exists to fix.
#[test]
fn a_record_that_cannot_be_read_does_not_cancel() {
    let dir = tempfile::tempdir().unwrap();
    let (runs, _id) = started(dir.path(), "a goal");
    assert_eq!(look(&runs, "no-such-run"), Look::Unreadable);
}

/// The whole point: a cancel written by what is a separate process in real use
/// reaches the flag the executor is watching.
#[tokio::test]
async fn a_cancel_from_another_connection_reaches_the_run() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("runs.sqlite");
    let (runs, id) = started(dir.path(), "a goal");
    let flag = Arc::new(AtomicBool::new(false));

    let watcher = tokio::spawn(watch(path.clone(), id.clone(), Arc::clone(&flag)));

    // What `timon runs cancel` does, on its own connection.
    Runs::open(&path)
        .unwrap()
        .cancel(&id, ALICE, NOW + 30)
        .unwrap();
    drop(runs);

    assert!(watcher.await.unwrap(), "the watcher set the flag");
    assert!(flag.load(Ordering::Relaxed), "the executor can see it");
}

#[tokio::test]
async fn the_watch_ends_when_the_run_finishes_on_its_own() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("runs.sqlite");
    let (runs, id) = started(dir.path(), "a goal");
    let flag = Arc::new(AtomicBool::new(false));

    let watcher = tokio::spawn(watch(path.clone(), id.clone(), Arc::clone(&flag)));
    runs.settle(&id, Status::Finished, NOW + 40, None).unwrap();

    assert!(!watcher.await.unwrap(), "no flag was set");
    assert!(!flag.load(Ordering::Relaxed), "the run was not cancelled");
}

/// Ctrl-C and a store cancel share one flag, so whichever happens first ends
/// the watch and the other is not waited on.
#[tokio::test]
async fn a_flag_already_set_ends_the_watch() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("runs.sqlite");
    let (_runs, id) = started(dir.path(), "a goal");
    let flag = Arc::new(AtomicBool::new(true));

    assert!(
        !watch(path, id, Arc::clone(&flag)).await,
        "it did not claim the cancellation as its own"
    );
}

/// The terminal status a stopped run records.
///
/// Four paths out of execution settled `Status::Finished` unconditionally, so a
/// cancelled run was recorded as having completed and `timon runs list` printed
/// `finished` beside work nobody received. Found by cancelling a real run with a
/// fake worker rather than by any test, which is why there is one now.
#[test]
fn a_cancelled_run_is_not_recorded_as_finished() {
    use timon::run::execute::terminal;

    let quiet = AtomicBool::new(false);
    assert_eq!(terminal(&quiet), Status::Finished);

    let stopped = AtomicBool::new(true);
    assert_eq!(
        terminal(&stopped),
        Status::Cancelled,
        "a run stopped by Ctrl-C or `timon runs cancel` did not finish"
    );
}
