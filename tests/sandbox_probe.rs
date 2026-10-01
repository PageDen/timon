//! Measuring the write sandbox, and refusing to pass when nothing was measured.
//!
//! The first real run of the probe passed `-C` to `codex sandbox`, which that
//! version rejects, so not one check executed — and six reported `blocked`,
//! because a command that never runs writes nothing. These tests pin the guard
//! that makes that impossible.

use std::path::PathBuf;

use timon::qualify::Observed;
use timon::run::execute::{Sandbox, worker_command};
use timon::sandbox_probe::{RAN, SANDBOX_SETTINGS, probe, sandboxed, surviving};

/// A scratch directory that is not under /tmp, which the probe refuses.
fn scratch(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("sandbox-probe")
        .join(format!("{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// The failure that was actually observed: a sandbox command that never ran.
/// `false` stands in for a Codex whose `sandbox` invocation is rejected.
#[tokio::test]
async fn a_sandbox_that_never_runs_cannot_qualify() {
    let dir = scratch("never-runs");
    let q = probe("false", &dir, "test-host", 0)
        .await
        .expect("the lab builds");

    assert!(
        !q.passed(),
        "a probe where nothing ran must not open the gate"
    );
    for f in &q.findings {
        let short_circuit = f
            .evidence
            .contains("no pooled credential store on this host");
        assert!(
            f.observed == Observed::NotRun || short_circuit,
            "{:?} reported {:?} although nothing ran: {}",
            f.check,
            f.observed,
            f.evidence
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_scratch_directory_under_tmp_is_refused() {
    // The write sandbox allows /tmp, so a lab there would measure that
    // allowance rather than the worktree boundary.
    let refused = probe("false", std::path::Path::new("/tmp/timon-probe"), "h", 0).await;
    assert!(refused.is_err());
}

#[test]
fn every_check_announces_that_it_ran() {
    let args = sandboxed("codex", "true");
    assert!(
        args.last()
            .is_some_and(|script| script.starts_with(&format!("echo {RAN};"))),
        "got {args:?}"
    );
}

/// `-C` made Codex 0.155.1 reject the whole invocation.
#[test]
fn the_sandbox_is_not_given_a_directory_flag() {
    let args = sandboxed("codex", "true");
    assert!(
        !args.iter().any(|a| a == "-C" || a == "--cd"),
        "got {args:?}"
    );
}

/// The probe must measure the sandbox a writing worker actually gets. If the
/// two drift apart, a pass here says nothing about real workers.
#[test]
fn the_probe_measures_the_sandbox_workers_get() {
    let worker: Vec<String> = worker_command("127.0.0.1:1456", None, Sandbox::WorkspaceWrite)
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert!(
        worker
            .windows(2)
            .any(|w| w[0] == "-s" && w[1] == "workspace-write"),
        "got {worker:?}"
    );
    assert!(SANDBOX_SETTINGS.contains(&"sandbox_mode=\"workspace-write\""));
    for setting in SANDBOX_SETTINGS
        .iter()
        .filter(|s| !s.starts_with("sandbox_mode"))
    {
        assert!(
            worker.iter().any(|a| a == setting),
            "{setting} is probed but not given to workers: {worker:?}"
        );
    }
}

/// The process check can only say "gone" if it can see one that is not.
#[test]
fn a_surviving_process_is_seen() {
    let marker = format!("287.{}", std::process::id() % 100_000);
    let mut child = std::process::Command::new("sleep")
        .arg(&marker)
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(200));
    let found = surviving(&format!("sleep {marker}"));
    let _ = child.kill();
    let _ = child.wait();
    assert_eq!(
        found,
        vec![child.id()],
        "the detector missed a live process"
    );
}
