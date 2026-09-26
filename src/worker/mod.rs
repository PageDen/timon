//! Bounded worker processes.
//!
//! A worker is a child process that receives its task on stdin, runs under a
//! wall-clock deadline in its own process group, and has its stdout and stderr
//! captured to private, size-bounded files. The supervisor never passes the task
//! on the command line, so it is not visible in the process list.

mod process;
pub mod slots;

pub use process::run_worker;

use serde::Serialize;
use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

/// Default maximum task size in bytes.
pub const DEFAULT_MAX_TASK_BYTES: usize = 64 * 1024;
/// Default per-stream capture limit in bytes.
pub const DEFAULT_MAX_OUTPUT_BYTES: u64 = 1024 * 1024;
/// Default time allowed to finish reading output after the worker exits.
pub const DEFAULT_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
/// Default time allowed to reap a killed worker.
pub const DEFAULT_REAP_TIMEOUT: Duration = Duration::from_secs(5);

/// Resource limits for one worker attempt.
#[derive(Clone, Debug)]
pub struct WorkerLimits {
    /// Maximum task size in bytes.
    pub max_task_bytes: usize,
    /// Wall-clock limit for the whole attempt, including task delivery.
    pub deadline: Duration,
    /// Maximum bytes written to each capture file. Output beyond this is
    /// read and discarded so the worker never blocks on a full pipe.
    pub max_output_bytes: u64,
    /// Time allowed to finish reading output after the worker exits.
    pub drain_timeout: Duration,
    /// Time allowed to reap the worker after it is killed.
    pub reap_timeout: Duration,
}

impl WorkerLimits {
    pub fn with_deadline(deadline: Duration) -> Self {
        Self {
            max_task_bytes: DEFAULT_MAX_TASK_BYTES,
            deadline,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            drain_timeout: DEFAULT_DRAIN_TIMEOUT,
            reap_timeout: DEFAULT_REAP_TIMEOUT,
        }
    }
}

/// Everything needed to launch one worker attempt.
#[derive(Clone, Debug)]
pub struct WorkerSpec {
    /// Executable to run. It is spawned directly, never through a shell.
    pub program: PathBuf,
    /// Arguments. The task must not be placed here.
    pub args: Vec<OsString>,
    /// Working directory. Inherited when `None`.
    pub cwd: Option<PathBuf>,
    /// Environment variables to set for the worker.
    pub env_set: Vec<(OsString, OsString)>,
    /// Environment variables to remove for the worker.
    pub env_remove: Vec<OsString>,
    /// Task text, delivered on stdin.
    pub task: String,
    /// Directory for `stdout.log` and `stderr.log`. Created with mode 0700 if
    /// missing; an existing directory must not be accessible to group or
    /// others. The capture files must not already exist.
    pub output_dir: PathBuf,
    pub limits: WorkerLimits,
}

/// How the worker process ended.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct ExitInfo {
    /// Exit code, if the worker exited normally.
    pub code: Option<i32>,
    /// Terminating signal, if the worker was killed by a signal.
    pub signal: Option<i32>,
}

/// Capture result for one output stream.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct StreamCapture {
    pub path: PathBuf,
    /// Bytes written to the capture file.
    pub bytes_written: u64,
    /// Bytes read from the worker, including discarded bytes.
    pub bytes_seen: u64,
    /// True when output exceeded the capture limit.
    pub truncated: bool,
}

/// Result of one worker attempt.
#[derive(Clone, Debug, Serialize)]
pub struct WorkerOutcome {
    pub exit: ExitInfo,
    /// The deadline expired and the worker was killed.
    pub timed_out: bool,
    /// The attempt was cancelled and the worker was killed.
    pub cancelled: bool,
    /// The whole task was written to the worker's stdin.
    pub input_complete: bool,
    /// Output could not be fully read, for example because a process outside
    /// the worker's process group kept a pipe open. Counts are partial.
    pub output_incomplete: bool,
    pub stdout: StreamCapture,
    pub stderr: StreamCapture,
    pub duration_ms: u64,
}

impl WorkerOutcome {
    /// True when the worker exited with code 0 and was not interrupted.
    pub fn succeeded(&self) -> bool {
        !self.timed_out && !self.cancelled && self.exit.code == Some(0)
    }
}

/// Reasons a worker could not be started.
#[derive(Debug)]
pub enum SpecError {
    EmptyTask,
    TaskTooLarge { bytes: usize, limit: usize },
    ZeroDeadline,
    OutputDirNotPrivate(PathBuf),
    OutputDirNotDirectory(PathBuf),
}

impl std::fmt::Display for SpecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpecError::EmptyTask => write!(f, "task is empty"),
            SpecError::TaskTooLarge { bytes, limit } => {
                write!(f, "task is {bytes} bytes; the limit is {limit}")
            }
            SpecError::ZeroDeadline => write!(f, "deadline must be greater than zero"),
            SpecError::OutputDirNotPrivate(path) => write!(
                f,
                "output directory {} must not be accessible to group or others",
                path.display()
            ),
            SpecError::OutputDirNotDirectory(path) => {
                write!(f, "output path {} is not a directory", path.display())
            }
        }
    }
}

impl std::error::Error for SpecError {}

impl WorkerSpec {
    /// Checks the parts of the spec that can be checked without side effects.
    pub fn validate(&self) -> Result<(), SpecError> {
        if self.task.is_empty() {
            return Err(SpecError::EmptyTask);
        }
        if self.task.len() > self.limits.max_task_bytes {
            return Err(SpecError::TaskTooLarge {
                bytes: self.task.len(),
                limit: self.limits.max_task_bytes,
            });
        }
        if self.limits.deadline.is_zero() {
            return Err(SpecError::ZeroDeadline);
        }
        Ok(())
    }
}
