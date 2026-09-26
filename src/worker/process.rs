// Adapted from Prodex (https://github.com/christiandoxa/prodex),
// crates/prodex-app/src/runtime_tools/sub_agent_process.rs, Apache-2.0.
// Modified: the task is delivered on stdin instead of argv, the attempt runs
// under a deadline that also covers task delivery, output is captured to
// private size-bounded files instead of being relayed, cancellation is a
// caller-supplied future, and Windows job-object support was removed.

use super::{ExitInfo, SpecError, StreamCapture, WorkerOutcome, WorkerSpec};
use anyhow::{Context, Result};
use std::fs::{self, OpenOptions};
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin, Command};

const READ_BUFFER_BYTES: usize = 16 * 1024;

enum Ending {
    Exited(std::process::ExitStatus),
    TimedOut,
    Cancelled,
}

/// Runs one worker attempt to completion.
///
/// The attempt ends when the worker exits, when the deadline expires, or when
/// `cancel` resolves, whichever comes first. On timeout or cancellation the
/// worker's whole process group is killed and reaped. After a normal exit any
/// remaining processes in the group are killed as well, so background
/// descendants cannot outlive the attempt or hold its output pipes open.
pub async fn run_worker<C>(spec: &WorkerSpec, cancel: C) -> Result<WorkerOutcome>
where
    C: Future<Output = ()>,
{
    spec.validate()?;
    let started = Instant::now();
    let deadline = tokio::time::Instant::from_std(started + spec.limits.deadline);

    prepare_output_dir(&spec.output_dir)?;
    let stdout_path = spec.output_dir.join("stdout.log");
    let stderr_path = spec.output_dir.join("stderr.log");
    let stdout_file = create_private_file(&stdout_path)?;
    let stderr_file = create_private_file(&stderr_path)?;

    let mut command = Command::new(&spec.program);
    command
        .args(&spec.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .process_group(0);
    if let Some(cwd) = &spec.cwd {
        command.current_dir(cwd);
    }
    for key in &spec.env_remove {
        command.env_remove(key);
    }
    for (key, value) in &spec.env_set {
        command.env(key, value);
    }

    let mut child = command
        .spawn()
        .with_context(|| format!("failed to start worker {}", spec.program.display()))?;
    let process_group = child.id();

    let stdin = child.stdin.take().context("worker stdin pipe missing")?;
    let stdout = child.stdout.take().context("worker stdout pipe missing")?;
    let stderr = child.stderr.take().context("worker stderr pipe missing")?;

    let input_task = tokio::spawn(deliver_task(stdin, spec.task.clone().into_bytes()));
    let stdout_state = Arc::new(CaptureState::default());
    let stderr_state = Arc::new(CaptureState::default());
    let stdout_task = tokio::spawn(capture_stream(
        stdout,
        tokio::fs::File::from_std(stdout_file),
        spec.limits.max_output_bytes,
        Arc::clone(&stdout_state),
    ));
    let stderr_task = tokio::spawn(capture_stream(
        stderr,
        tokio::fs::File::from_std(stderr_file),
        spec.limits.max_output_bytes,
        Arc::clone(&stderr_state),
    ));

    tokio::pin!(cancel);
    let ending = tokio::select! {
        status = child.wait() => Ending::Exited(status.context("failed to wait for worker")?),
        () = tokio::time::sleep_until(deadline) => Ending::TimedOut,
        () = &mut cancel => Ending::Cancelled,
    };

    let (status, timed_out, cancelled) = match ending {
        Ending::Exited(status) => {
            // Remove any descendants still running in the worker's group.
            kill_process_group(process_group);
            (status, false, false)
        }
        Ending::TimedOut => (
            kill_and_reap(&mut child, process_group, spec).await?,
            true,
            false,
        ),
        Ending::Cancelled => (
            kill_and_reap(&mut child, process_group, spec).await?,
            false,
            true,
        ),
    };

    let input_complete = finish_input(input_task, spec).await;
    let output_incomplete = drain_output(stdout_task, stderr_task, spec).await;

    Ok(WorkerOutcome {
        exit: exit_info(status),
        timed_out,
        cancelled,
        input_complete,
        output_incomplete,
        stdout: stdout_state.summary(stdout_path),
        stderr: stderr_state.summary(stderr_path),
        duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
    })
}

fn prepare_output_dir(dir: &Path) -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    match fs::symlink_metadata(dir) {
        Ok(metadata) => {
            if !metadata.is_dir() {
                return Err(SpecError::OutputDirNotDirectory(dir.to_path_buf()).into());
            }
            if metadata.permissions().mode() & 0o077 != 0 {
                return Err(SpecError::OutputDirNotPrivate(dir.to_path_buf()).into());
            }
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .with_context(|| format!("failed to create output directory {}", dir.display())),
        Err(error) => Err(error)
            .with_context(|| format!("failed to inspect output directory {}", dir.display())),
    }
}

fn create_private_file(path: &Path) -> Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("failed to create capture file {}", path.display()))
}

/// Writes the task to the worker's stdin and closes it.
///
/// Returns false if the worker closed stdin before reading everything.
async fn deliver_task(mut stdin: ChildStdin, task: Vec<u8>) -> bool {
    if stdin.write_all(&task).await.is_err() {
        return false;
    }
    stdin.shutdown().await.is_ok()
}

async fn finish_input(input_task: tokio::task::JoinHandle<bool>, spec: &WorkerSpec) -> bool {
    // Once the worker's group is gone the pipe has no reader, so a pending
    // write fails promptly. The timeout covers a reader that escaped the group.
    let mut input_task = input_task;
    match tokio::time::timeout(spec.limits.drain_timeout, &mut input_task).await {
        Ok(Ok(complete)) => complete,
        Ok(Err(_)) => false,
        Err(_) => {
            input_task.abort();
            false
        }
    }
}

#[derive(Default)]
struct CaptureState {
    bytes_written: AtomicU64,
    bytes_seen: AtomicU64,
    truncated: AtomicBool,
}

impl CaptureState {
    fn summary(&self, path: PathBuf) -> StreamCapture {
        StreamCapture {
            path,
            bytes_written: self.bytes_written.load(Ordering::Acquire),
            bytes_seen: self.bytes_seen.load(Ordering::Acquire),
            truncated: self.truncated.load(Ordering::Acquire),
        }
    }
}

/// Copies up to `limit` bytes into `file` and discards the rest, reading until
/// end of stream so the worker never blocks on a full pipe.
async fn capture_stream<R>(
    mut reader: R,
    mut file: tokio::fs::File,
    limit: u64,
    state: Arc<CaptureState>,
) -> io::Result<()>
where
    R: AsyncRead + Unpin,
{
    let mut buffer = vec![0_u8; READ_BUFFER_BYTES];
    let mut written = 0_u64;
    let mut write_error = None;
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        state.bytes_seen.fetch_add(read as u64, Ordering::AcqRel);
        let room = limit.saturating_sub(written);
        let keep = usize::try_from(room).map_or(read, |room| room.min(read));
        if keep < read {
            state.truncated.store(true, Ordering::Release);
        }
        if keep > 0 && write_error.is_none() {
            match file.write_all(&buffer[..keep]).await {
                Ok(()) => {
                    written += keep as u64;
                    state.bytes_written.store(written, Ordering::Release);
                }
                Err(error) => write_error = Some(error),
            }
        }
    }
    if let Some(error) = write_error {
        return Err(error);
    }
    file.flush().await?;
    file.sync_all().await
}

/// Waits for both capture tasks. Returns true if output is incomplete.
async fn drain_output(
    mut stdout_task: tokio::task::JoinHandle<io::Result<()>>,
    mut stderr_task: tokio::task::JoinHandle<io::Result<()>>,
    spec: &WorkerSpec,
) -> bool {
    let drained = tokio::time::timeout(spec.limits.drain_timeout, async {
        let (stdout, stderr) = tokio::join!(&mut stdout_task, &mut stderr_task);
        matches!(stdout, Ok(Ok(()))) && matches!(stderr, Ok(Ok(())))
    })
    .await;
    match drained {
        Ok(complete) => !complete,
        Err(_) => {
            stdout_task.abort();
            stderr_task.abort();
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            true
        }
    }
}

async fn kill_and_reap(
    child: &mut Child,
    process_group: Option<u32>,
    spec: &WorkerSpec,
) -> Result<std::process::ExitStatus> {
    if !kill_process_group(process_group) && child.try_wait()?.is_none() {
        child.start_kill().context("failed to kill worker")?;
    }
    tokio::time::timeout(spec.limits.reap_timeout, child.wait())
        .await
        .context("timed out reaping killed worker")?
        .context("failed to reap killed worker")
}

/// Sends SIGKILL to the worker's process group. Returns true if a signal was
/// delivered to at least one process.
///
/// After the group leader has been reaped this is best effort: if the group is
/// already empty the call fails with ESRCH and nothing happens.
fn kill_process_group(process_group: Option<u32>) -> bool {
    let Some(process_group) = process_group.and_then(|id| libc::pid_t::try_from(id).ok()) else {
        return false;
    };
    if process_group <= 0 {
        return false;
    }
    // SAFETY: kill(2) with a negative pid signals the process group; it does
    // not touch memory owned by this process.
    unsafe { libc::kill(-process_group, libc::SIGKILL) == 0 }
}

fn exit_info(status: std::process::ExitStatus) -> ExitInfo {
    use std::os::unix::process::ExitStatusExt;
    ExitInfo {
        code: status.code(),
        signal: status.signal(),
    }
}
