//! A run watching its own record, so a cancel from another process reaches it.
//!
//! `timon runs cancel` writes `Cancelling` to the store and revokes the run's
//! grant. That stops the broker authorising anything new, but it does not stop
//! what is already running: the worker's turn continues to completion in a
//! process that never reads the record. Ctrl-C has always worked, because the
//! executor watches a flag in its own process. A cancel from a second terminal
//! did not, which is the gap TODO.md recorded.
//!
//! So the run polls its own row and sets the same flag the signal handler sets.
//! Everything downstream — killing the worker's process group, declining to
//! start further tasks — is already built and needs no change.
//!
//! **A read failure never cancels.** The flag is set only on a definite reading
//! of `Cancelling` from the store. A transient error is logged and retried,
//! because a watcher that cancelled on a failed read would turn a brief lock
//! contention into a killed run, which is a worse failure than the one it
//! exists to fix.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use super::record::{Run, RunError, Runs, Status};

/// How often the record is read. A cancel is a human action and a second is
/// faster than the hand that typed it; polling harder would mean more contention
/// on a store that a run is already writing to.
pub const POLL: Duration = Duration::from_secs(1);

/// What a single look at the record concluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Look {
    /// The run is still live and was not cancelled.
    Continue,
    /// The record says cancelling. Set the flag.
    Cancel,
    /// The run has reached a terminal state, so there is nothing left to watch.
    Finished,
    /// The record could not be read. Try again; do not cancel.
    Unreadable,
}

/// Reads one run's status and says what to do about it.
///
/// Separated from the polling loop so the decision is testable without a
/// runtime, a worker or a model call.
pub fn look(store: &Runs, id: &str) -> Look {
    match store.get(id) {
        Ok(run) => classify(&run),
        // A record that has vanished is not a reason to kill a running worker:
        // it means the store was moved or rebuilt underneath us. Treated as
        // unreadable rather than as a cancellation.
        Err(RunError::Unknown(_)) | Err(RunError::Db(_)) => Look::Unreadable,
        Err(_) => Look::Unreadable,
    }
}

fn classify(run: &Run) -> Look {
    match run.status {
        Status::Cancelling => Look::Cancel,
        status if status.live() => Look::Continue,
        // Terminal, or `Recorded` — a preflight that never ran. Either way
        // there is nothing left to watch and nothing that could be cancelled.
        _ => Look::Finished,
    }
}

/// Polls the record until the run is cancelled or finishes.
///
/// Returns true if it set the flag. Intended to be spawned beside the signal
/// handler; it owns its own connection because the executor is using the other
/// one.
pub async fn watch(path: std::path::PathBuf, id: String, cancel: Arc<AtomicBool>) -> bool {
    // Opening the store here rather than taking a handle: `Runs` holds a
    // `Connection`, which is not `Sync`, and sharing one across the executor
    // and a watcher would serialise them behind a lock for no gain. WAL makes a
    // second reader free.
    let store = match Runs::open(&path) {
        Ok(store) => store,
        Err(error) => {
            eprintln!("timon: cannot watch {id} for cancellation: {error}");
            return false;
        }
    };

    loop {
        // Already stopped by Ctrl-C or by the executor finishing.
        if cancel.load(Ordering::Relaxed) {
            return false;
        }
        match look(&store, &id) {
            Look::Cancel => {
                eprintln!("timon: {id} was cancelled; stopping the run and its worker…");
                cancel.store(true, Ordering::Relaxed);
                return true;
            }
            Look::Finished => return false,
            Look::Continue | Look::Unreadable => {}
        }
        tokio::time::sleep(POLL).await;
    }
}
