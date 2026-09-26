//! Producer-side delivery: the spool, and what survives a crash.
//!
//! Each test runs its own recorder on a private socket, so these exercise the
//! real protocol rather than a stub. What they cannot show is cross-account
//! attribution on replay, which needs separate UIDs — `tests/cross-user.sh`
//! covers that.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::json;
use timon::recorder::event::UsageEvent;
use timon::recorder::producer::{Delivery, Spool, deliver, replay};
use timon::recorder::protocol::{Request, Response};
use timon::recorder::server::{Config, serve};
use tokio::sync::Notify;

/// A recorder listening on a socket of its own, shut down when dropped.
struct Recorder {
    socket: PathBuf,
    stop: Arc<Notify>,
    handle: tokio::task::JoinHandle<()>,
    _dir: tempfile::TempDir,
}

impl Recorder {
    async fn start() -> Recorder {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("usage.sock");
        let mut config = Config::new(socket.clone(), dir.path().join("usage.db"));
        // Read our own rows back without needing a second account.
        config.admin_uids = vec![unsafe { libc::getuid() }];
        let stop = Arc::new(Notify::new());
        let waiter = Arc::clone(&stop);
        let handle = tokio::spawn(async move {
            serve(config, async move { waiter.notified().await })
                .await
                .unwrap();
        });
        // Wait for the socket rather than sleeping a fixed time.
        for _ in 0..200 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        Recorder {
            socket,
            stop,
            handle,
            _dir: dir,
        }
    }

    async fn stop(self) {
        self.stop.notify_one();
        let _ = self.handle.await;
    }

    async fn rows(&self) -> Vec<serde_json::Value> {
        let response = timon::recorder::client::send(
            &self.socket,
            &Request::Query {
                since: None,
                until: None,
                only_uid: None,
                limit: Some(500),
            },
        )
        .await
        .unwrap();
        match response {
            Response::Rows { rows, .. } => rows
                .into_iter()
                .map(|r| serde_json::to_value(r).unwrap())
                .collect(),
            other => panic!("expected rows, got {other:?}"),
        }
    }
}

fn spool(dir: &Path, max: usize) -> Spool {
    Spool::open(dir.to_path_buf(), max).unwrap()
}

fn event(id: &str, output: u64) -> UsageEvent {
    serde_json::from_value(json!({
        "version": 1,
        "client_event_id": id,
        "run_id": "run-1",
        "attempt_id": "1",
        "role": "worker",
        "usage": { "input": 100, "output": output },
        "usage_status": "complete",
        "occurred_at": 1_790_000_000
    }))
    .unwrap()
}

#[tokio::test]
async fn a_delivered_event_is_removed_from_the_spool() {
    let recorder = Recorder::start().await;
    let dir = tempfile::tempdir().unwrap();
    let spool = spool(dir.path(), 16);

    let delivery = deliver(&recorder.socket, &spool, &event("e1", 10)).await;

    assert!(matches!(
        delivery,
        Delivery::Recorded {
            duplicate: false,
            ..
        }
    ));
    assert!(
        spool.pending().unwrap().is_empty(),
        "a committed event must not stay spooled"
    );
    assert_eq!(recorder.rows().await.len(), 1);
    recorder.stop().await;
}

#[tokio::test]
async fn an_unreachable_recorder_leaves_the_event_spooled() {
    let dir = tempfile::tempdir().unwrap();
    let spool = spool(dir.path(), 16);
    let missing = dir.path().join("nothing.sock");

    let delivery = deliver(&missing, &spool, &event("e1", 10)).await;

    assert!(matches!(delivery, Delivery::Spooled { .. }));
    assert_eq!(
        spool.pending().unwrap().len(),
        1,
        "the event must survive for replay"
    );
}

#[tokio::test]
async fn an_outage_then_a_replay_records_the_event_once() {
    let dir = tempfile::tempdir().unwrap();
    let spool = spool(dir.path(), 16);
    let missing = dir.path().join("nothing.sock");
    deliver(&missing, &spool, &event("e1", 10)).await;

    let recorder = Recorder::start().await;
    let outcome = replay(&recorder.socket, &spool, 32).await.unwrap();

    assert_eq!(outcome.delivered, 1);
    assert_eq!(outcome.still_pending, 0);
    assert!(spool.pending().unwrap().is_empty());
    assert_eq!(recorder.rows().await.len(), 1);
    recorder.stop().await;
}

#[tokio::test]
async fn a_crash_after_the_commit_but_before_the_spool_is_cleared_does_not_double_count() {
    // The dangerous window: the daemon committed, the acknowledgement never got
    // used, so the producer still holds the event and will send it again.
    let recorder = Recorder::start().await;
    let dir = tempfile::tempdir().unwrap();
    let spool = spool(dir.path(), 16);
    let event = event("e1", 10);

    let staged = spool.stage(&event).unwrap();
    timon::recorder::client::send(
        &recorder.socket,
        &Request::Append {
            event: Box::new(event.clone()),
        },
    )
    .await
    .unwrap();
    assert!(
        staged.exists(),
        "simulating a crash before the file was removed"
    );

    let outcome = replay(&recorder.socket, &spool, 32).await.unwrap();

    assert_eq!(
        outcome.already_present, 1,
        "the retry must be recognised, not stored"
    );
    assert_eq!(outcome.delivered, 0);
    assert_eq!(recorder.rows().await.len(), 1, "one attempt, one row");
    recorder.stop().await;
}

#[tokio::test]
async fn two_replays_racing_store_the_event_once() {
    let recorder = Recorder::start().await;
    let dir = tempfile::tempdir().unwrap();
    let spool = spool(dir.path(), 16);
    for n in 0..8 {
        spool.stage(&event(&format!("e{n}"), 10)).unwrap();
    }

    let (left, right) = tokio::join!(
        replay(&recorder.socket, &spool, 32),
        replay(&recorder.socket, &spool, 32)
    );
    let (left, right) = (left.unwrap(), right.unwrap());

    // Two concurrent passes will both see the same pending files, so each event
    // is legitimately handled twice: one pass inserts it, the other recognises a
    // duplicate. What must hold is that it is *stored* once.
    assert_eq!(
        left.delivered + right.delivered,
        8,
        "each event must be newly stored exactly once"
    );
    assert_eq!(
        recorder.rows().await.len(),
        8,
        "no duplicates from the race"
    );
    assert!(
        spool.pending().unwrap().is_empty(),
        "the spool is drained either way"
    );
    recorder.stop().await;
}

#[tokio::test]
async fn a_full_spool_records_a_gap_rather_than_losing_the_event_silently() {
    let dir = tempfile::tempdir().unwrap();
    let spool = spool(dir.path(), 2);
    let missing = dir.path().join("nothing.sock");

    for n in 0..5 {
        deliver(&missing, &spool, &event(&format!("e{n}"), 10)).await;
    }

    assert_eq!(spool.pending().unwrap().len(), 2, "the bound is respected");
    let gap = spool.read_gap().unwrap().expect("a gap must be recorded");
    assert_eq!(gap.dropped, 3, "every dropped event is counted");
    assert_eq!(gap.first_occurred_at, Some(1_790_000_000));
}

#[tokio::test]
async fn the_gap_marker_coalesces_instead_of_growing_with_every_loss() {
    // A long outage must not turn the marker itself into an unbounded pile of
    // files, which is why drops accumulate into one record.
    let dir = tempfile::tempdir().unwrap();
    let spool = spool(dir.path(), 1);
    let missing = dir.path().join("nothing.sock");

    for n in 0..50 {
        deliver(&missing, &spool, &event(&format!("e{n}"), 10)).await;
    }

    let files = std::fs::read_dir(dir.path()).unwrap().count();
    assert!(
        files <= 3,
        "expected the pending event plus one gap file, found {files}"
    );
    assert_eq!(spool.read_gap().unwrap().unwrap().dropped, 49);
}

#[tokio::test]
async fn a_corrupt_spool_file_is_set_aside_and_counted_instead_of_retried_forever() {
    let recorder = Recorder::start().await;
    let dir = tempfile::tempdir().unwrap();
    let spool = spool(dir.path(), 16);
    std::fs::write(dir.path().join("broken.json"), b"{not json").unwrap();

    let outcome = replay(&recorder.socket, &spool, 32).await.unwrap();

    assert_eq!(outcome.corrupt, 1);
    assert!(
        spool.pending().unwrap().is_empty(),
        "it must not block later replays"
    );
    assert!(
        dir.path().join("broken.corrupt").exists(),
        "kept for inspection"
    );
    recorder.stop().await;
}

#[tokio::test]
async fn a_partly_written_file_is_ignored_until_it_is_complete() {
    // Staging renames into place, so an incomplete write is only ever visible
    // under a temporary name.
    let dir = tempfile::tempdir().unwrap();
    let spool = spool(dir.path(), 16);
    std::fs::write(dir.path().join("half.partial"), b"{\"version\":1").unwrap();

    assert!(
        spool.pending().unwrap().is_empty(),
        "a partial file is not a deliverable event"
    );
}

#[tokio::test]
async fn a_retry_of_the_same_attempt_reuses_one_spool_slot() {
    let dir = tempfile::tempdir().unwrap();
    let spool = spool(dir.path(), 16);
    let missing = dir.path().join("nothing.sock");

    for _ in 0..4 {
        deliver(&missing, &spool, &event("same-attempt", 10)).await;
    }

    assert_eq!(
        spool.pending().unwrap().len(),
        1,
        "one attempt is one pending event, however many times delivery was tried"
    );
}

#[tokio::test]
async fn the_spool_directory_and_its_files_are_private() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let nested = dir.path().join("state/spool");
    let spool = Spool::open(nested.clone(), 16).unwrap();
    let path = spool.stage(&event("e1", 10)).unwrap();

    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        mode(&nested),
        0o700,
        "another account must not read the spool"
    );
    assert_eq!(mode(&path), 0o600);
}

#[tokio::test]
async fn a_replayed_event_keeps_the_time_it_was_observed() {
    let dir = tempfile::tempdir().unwrap();
    let spool = spool(dir.path(), 16);
    let missing = dir.path().join("nothing.sock");
    deliver(&missing, &spool, &event("e1", 10)).await;

    let recorder = Recorder::start().await;
    replay(&recorder.socket, &spool, 32).await.unwrap();

    let rows = recorder.rows().await;
    assert_eq!(
        rows[0]["occurred_at"], 1_790_000_000,
        "a later delivery must not restamp when the work happened"
    );
    assert!(rows[0]["received_at"].as_i64().unwrap() > 1_790_000_000);
    recorder.stop().await;
}
