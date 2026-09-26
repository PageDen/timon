//! Worker supervisor tests. Every worker here is a small `/bin/sh` script, so
//! no model provider is involved.

use std::num::NonZeroU16;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use timon::worker::slots::{SlotError, SlotMode, SlotPool};
use timon::worker::{WorkerLimits, WorkerSpec, run_worker};

fn sh(script: &str, task: &str, output_dir: &Path, deadline: Duration) -> WorkerSpec {
    WorkerSpec {
        program: PathBuf::from("/bin/sh"),
        args: vec!["-c".into(), script.into()],
        cwd: None,
        env_set: Vec::new(),
        env_remove: Vec::new(),
        task: task.to_owned(),
        output_dir: output_dir.to_path_buf(),
        limits: WorkerLimits::with_deadline(deadline),
    }
}

fn never() -> std::future::Pending<()> {
    std::future::pending()
}

/// True if the process exists and is not a zombie.
fn process_alive(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => {
            // The state field follows the parenthesised command name.
            let state = stat
                .rsplit_once(')')
                .and_then(|(_, rest)| rest.trim().chars().next());
            state != Some('Z')
        }
        Err(_) => false,
    }
}

async fn wait_until_dead(pid: u32) -> bool {
    for _ in 0..50 {
        if !process_alive(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

fn read_pid(path: &Path) -> u32 {
    for _ in 0..100 {
        if let Ok(text) = std::fs::read_to_string(path)
            && let Ok(pid) = text.trim().parse()
        {
            return pid;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("pid file {} was not written", path.display());
}

#[tokio::test]
async fn delivers_large_unicode_task_intact_on_stdin() {
    let dir = tempfile::tempdir().unwrap();
    let unit = "line with 'quotes' \"double\" $VAR `tick` ñ 漢字 🚀\n";
    let mut task = String::new();
    while task.len() + unit.len() <= timon::worker::DEFAULT_MAX_TASK_BYTES {
        task.push_str(unit);
    }
    let out = dir.path().join("out");
    let spec = sh("cat", &task, &out, Duration::from_secs(10));

    let outcome = run_worker(&spec, never()).await.unwrap();

    assert!(outcome.succeeded(), "{outcome:?}");
    assert!(outcome.input_complete);
    assert!(!outcome.output_incomplete);
    assert_eq!(
        std::fs::read_to_string(out.join("stdout.log")).unwrap(),
        task
    );
}

#[tokio::test]
async fn task_is_not_visible_in_process_arguments() {
    let dir = tempfile::tempdir().unwrap();
    let marker = "secret-marker-7f3a9c";
    let out = dir.path().join("out");
    // Print our own command line, then consume stdin.
    let script = r#"tr '\0' ' ' < /proc/$$/cmdline; cat > /dev/null"#;
    let spec = sh(
        script,
        &format!("task containing {marker}"),
        &out,
        Duration::from_secs(10),
    );

    let outcome = run_worker(&spec, never()).await.unwrap();

    assert!(outcome.succeeded(), "{outcome:?}");
    let cmdline = std::fs::read_to_string(out.join("stdout.log")).unwrap();
    assert!(cmdline.contains("/bin/sh"), "unexpected cmdline: {cmdline}");
    assert!(!cmdline.contains(marker));
}

#[tokio::test]
async fn deadline_covers_blocked_task_delivery() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out");
    // Larger than a pipe buffer, and the worker never reads stdin.
    let task = "x".repeat(1024 * 1024);
    let mut spec = sh("sleep 30", &task, &out, Duration::from_millis(500));
    spec.limits.max_task_bytes = task.len();

    let started = Instant::now();
    let outcome = run_worker(&spec, never()).await.unwrap();

    assert!(outcome.timed_out, "{outcome:?}");
    assert!(!outcome.input_complete);
    assert_eq!(outcome.exit.signal, Some(libc::SIGKILL));
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn output_flood_is_bounded_and_does_not_block_the_worker() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out");
    let mut spec = sh(
        "cat > /dev/null; head -c 10000000 /dev/zero; head -c 300000 /dev/zero >&2",
        "go",
        &out,
        Duration::from_secs(20),
    );
    spec.limits.max_output_bytes = 64 * 1024;

    let outcome = run_worker(&spec, never()).await.unwrap();

    assert!(outcome.succeeded(), "{outcome:?}");
    assert!(outcome.stdout.truncated);
    assert_eq!(outcome.stdout.bytes_written, 64 * 1024);
    assert_eq!(outcome.stdout.bytes_seen, 10_000_000);
    assert!(outcome.stderr.truncated);
    assert_eq!(outcome.stderr.bytes_seen, 300_000);
    assert_eq!(
        std::fs::metadata(out.join("stdout.log")).unwrap().len(),
        64 * 1024
    );
}

#[tokio::test]
async fn timeout_kills_descendants() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out");
    let pid_file = dir.path().join("grandchild.pid");
    let script = format!("sleep 60 & echo $! > {}; wait", pid_file.display());
    let spec = sh(&script, "go", &out, Duration::from_millis(500));

    let outcome = run_worker(&spec, never()).await.unwrap();

    assert!(outcome.timed_out, "{outcome:?}");
    let grandchild = read_pid(&pid_file);
    assert!(
        wait_until_dead(grandchild).await,
        "grandchild {grandchild} survived"
    );
}

#[tokio::test]
async fn background_descendants_are_removed_after_normal_exit() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out");
    let pid_file = dir.path().join("grandchild.pid");
    // The background sleep inherits stdout, so without group cleanup the
    // output pipe would stay open for a minute.
    let script = format!(
        "cat > /dev/null; sleep 60 & echo $! > {}; exit 0",
        pid_file.display()
    );
    let spec = sh(&script, "go", &out, Duration::from_secs(20));

    let started = Instant::now();
    let outcome = run_worker(&spec, never()).await.unwrap();

    assert!(outcome.succeeded(), "{outcome:?}");
    assert!(!outcome.output_incomplete);
    assert!(started.elapsed() < Duration::from_secs(5));
    let grandchild = read_pid(&pid_file);
    assert!(
        wait_until_dead(grandchild).await,
        "grandchild {grandchild} survived"
    );
}

#[tokio::test]
async fn cancellation_kills_worker() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out");
    let spec = sh(
        "cat > /dev/null; sleep 60",
        "go",
        &out,
        Duration::from_secs(60),
    );

    let cancel = tokio::time::sleep(Duration::from_millis(300));
    let started = Instant::now();
    let outcome = run_worker(&spec, cancel).await.unwrap();

    assert!(outcome.cancelled, "{outcome:?}");
    assert!(!outcome.timed_out);
    assert!(!outcome.succeeded());
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn worker_that_ignores_stdin_is_reported_without_hanging() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out");
    let task = "y".repeat(512 * 1024);
    let mut spec = sh("exit 3", &task, &out, Duration::from_secs(10));
    spec.limits.max_task_bytes = task.len();

    let outcome = run_worker(&spec, never()).await.unwrap();

    assert_eq!(outcome.exit.code, Some(3));
    assert!(!outcome.input_complete);
    assert!(!outcome.timed_out);
    assert!(!outcome.succeeded());
}

#[tokio::test]
async fn reports_exit_code_and_stderr() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out");
    let spec = sh(
        "cat > /dev/null; echo oops >&2; exit 7",
        "go",
        &out,
        Duration::from_secs(10),
    );

    let outcome = run_worker(&spec, never()).await.unwrap();

    assert_eq!(outcome.exit.code, Some(7));
    assert_eq!(
        std::fs::read_to_string(out.join("stderr.log")).unwrap(),
        "oops\n"
    );
}

#[tokio::test]
async fn rejects_invalid_specs_before_starting() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out");
    let marker = dir.path().join("started");
    let script = format!("touch {}", marker.display());

    let empty = sh(&script, "", &out, Duration::from_secs(5));
    assert!(run_worker(&empty, never()).await.is_err());

    let mut oversized = sh(&script, "abcdef", &out, Duration::from_secs(5));
    oversized.limits.max_task_bytes = 5;
    assert!(run_worker(&oversized, never()).await.is_err());

    let zero_deadline = sh(&script, "go", &out, Duration::ZERO);
    assert!(run_worker(&zero_deadline, never()).await.is_err());

    assert!(!marker.exists());
}

#[tokio::test]
async fn capture_files_are_private_and_never_overwritten() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out");
    let spec = sh("cat", "hello", &out, Duration::from_secs(10));

    run_worker(&spec, never()).await.unwrap();

    let mode = |path: PathBuf| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(out.clone()), 0o700);
    assert_eq!(mode(out.join("stdout.log")), 0o600);
    assert_eq!(mode(out.join("stderr.log")), 0o600);

    // A second attempt into the same directory must not clobber the first.
    assert!(run_worker(&spec, never()).await.is_err());
    assert_eq!(
        std::fs::read_to_string(out.join("stdout.log")).unwrap(),
        "hello"
    );
}

#[tokio::test]
async fn rejects_output_directory_readable_by_others() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out");
    std::fs::create_dir(&out).unwrap();
    std::fs::set_permissions(&out, std::fs::Permissions::from_mode(0o755)).unwrap();
    let spec = sh("cat", "hello", &out, Duration::from_secs(10));

    assert!(run_worker(&spec, never()).await.is_err());
}

#[test]
fn slots_limit_concurrency_and_release_on_drop() {
    let dir = tempfile::tempdir().unwrap();
    let pool = SlotPool::new(
        dir.path(),
        NonZeroU16::new(2).unwrap(),
        SlotMode::CreateMissing,
    );

    let first = pool.try_acquire().unwrap();
    let second = pool.try_acquire().unwrap();
    assert_ne!(first.index(), second.index());
    assert!(matches!(pool.try_acquire(), Err(SlotError::Full)));

    drop(first);
    let third = pool.try_acquire().unwrap();
    assert!(matches!(pool.try_acquire(), Err(SlotError::Full)));
    drop(third);
    drop(second);

    // Slot files stay in place after release.
    assert!(pool.slot_path(0).exists());
    assert!(pool.slot_path(1).exists());
}

#[test]
fn a_slot_is_released_even_when_a_forked_child_shares_the_descriptor() {
    // The lock belongs to the open file description. A child forked by another
    // thread while the slot was held shares that description until it execs, so
    // releasing by closing our own descriptor alone would leave the slot locked
    // and the next caller would see `Full` while the slot is free.
    //
    // `dup` shares an open file description exactly the way `fork` does, so it
    // reproduces that window without depending on process timing.
    let dir = tempfile::tempdir().unwrap();
    let pool = SlotPool::new(
        dir.path(),
        NonZeroU16::new(1).unwrap(),
        SlotMode::CreateMissing,
    );

    let lease = pool.try_acquire().unwrap();
    let shared = unsafe { libc::dup(std::os::fd::AsRawFd::as_raw_fd(lease.file_for_test())) };
    assert!(
        shared >= 0,
        "dup failed: {}",
        std::io::Error::last_os_error()
    );

    drop(lease);

    let reacquired = pool.try_acquire();
    unsafe { libc::close(shared) };

    assert!(
        reacquired.is_ok(),
        "a released slot must be free even while a copy of the descriptor is open, got {:?}",
        reacquired.err()
    );
}

#[test]
fn create_missing_mode_creates_private_slot_directory() {
    let dir = tempfile::tempdir().unwrap();
    let slot_dir = dir.path().join("nested").join("slots");
    let pool = SlotPool::new(
        &slot_dir,
        NonZeroU16::new(1).unwrap(),
        SlotMode::CreateMissing,
    );

    let lease = pool.try_acquire().unwrap();

    assert_eq!(lease.index(), 0);
    let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&slot_dir), 0o700);
    assert_eq!(mode(&pool.slot_path(0)), 0o600);
}

#[test]
fn provisioned_slots_must_exist() {
    let dir = tempfile::tempdir().unwrap();
    let pool = SlotPool::new(
        dir.path(),
        NonZeroU16::new(1).unwrap(),
        SlotMode::Provisioned,
    );
    assert!(matches!(pool.try_acquire(), Err(SlotError::Missing(_))));

    std::fs::File::create(pool.slot_path(0)).unwrap();
    let lease = pool.try_acquire().unwrap();
    assert_eq!(lease.index(), 0);
}

#[test]
fn slot_is_released_when_holding_process_exits() {
    let dir = tempfile::tempdir().unwrap();
    let pool = SlotPool::new(
        dir.path(),
        NonZeroU16::new(1).unwrap(),
        SlotMode::CreateMissing,
    );
    drop(pool.try_acquire().unwrap());

    // Another process holds the slot, then exits.
    let script = format!(
        "exec 9>>{}; flock -n 9 || exit 1; sleep 0.5",
        pool.slot_path(0).display()
    );
    let mut holder = std::process::Command::new("/bin/sh")
        .args(["-c", &script])
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(200));
    assert!(matches!(pool.try_acquire(), Err(SlotError::Full)));

    assert!(holder.wait().unwrap().success());
    assert!(pool.try_acquire().is_ok());
}
