//! One attempt by the lead or by a worker.
//!
//! The lead and the workers differ in which model they route to and how much
//! they are trusted to decide, not in how they are supervised or accounted for.
//! Both run as a supervised child process, both write a typed result, and both
//! report usage through the same envelope. That is deliberate: per-user usage
//! tracking has to account for the strong lead model as well as the cheap
//! workers, and a worker-only envelope would leave the largest consumer out.

use crate::result::{self, ResultStatus};
use crate::usage::{self, Accumulation, UsageNote, UsageReport, UsageStatus, codex};
use crate::worker::{WorkerOutcome, WorkerSpec, run_worker};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::future::Future;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Which role an attempt plays.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// The strong model that plans, integrates and verifies.
    Lead,
    /// A cheaper model running one bounded task.
    Worker,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Lead => "lead",
            Role::Worker => "worker",
        }
    }
}

/// Where an attempt's usage events are read from.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "from", rename_all = "snake_case")]
pub enum UsageSource {
    /// No usage stream. The report is [`usage::UsageStatus::Unknown`].
    None,
    /// The captured stdout of the attempt, where `codex exec --json` writes.
    Stdout,
    /// The captured stderr of the attempt.
    Stderr,
    /// A file the child was told to write.
    File { path: PathBuf },
}

/// Everything needed to run one accounted attempt.
#[derive(Clone, Debug)]
pub struct AttemptSpec {
    pub role: Role,
    /// Identifies the run this attempt belongs to.
    pub run_id: String,
    /// Identifies this attempt within the run. A retry is a new attempt.
    pub attempt_id: String,
    /// Overrides the derived usage event id. Leave `None` unless an existing id
    /// has to be preserved.
    pub client_event_id: Option<String>,
    pub worker: WorkerSpec,
    /// File the child was told to write its final message to.
    pub result_file: Option<PathBuf>,
    /// Schema the child was given for that file.
    pub result_schema: Option<PathBuf>,
    pub usage_source: UsageSource,
    pub accumulation: Accumulation,
}

impl AttemptSpec {
    /// The usage event id for this attempt.
    ///
    /// The id is derived from the role and the two identifiers rather than
    /// generated, so a retry of the same attempt, or a replay after a restart,
    /// produces the same id. Durable usage recording deduplicates on it, and a
    /// freshly generated id on every send would defeat that.
    pub fn client_event_id(&self) -> String {
        match &self.client_event_id {
            Some(id) => id.clone(),
            None => format!("{}:{}:{}", self.role.as_str(), self.run_id, self.attempt_id),
        }
    }
}

/// One attempt's process outcome, typed result and normalized usage.
#[derive(Clone, Debug, Serialize)]
pub struct AttemptReport {
    pub run_id: String,
    pub attempt_id: String,
    /// Stable id for this attempt's usage event.
    pub client_event_id: String,
    pub role: Role,
    /// When the attempt started, as reported by this host's clock. Kept as
    /// recorded: a later replay does not restamp it.
    pub occurred_at_unix_ms: u64,
    pub process: WorkerOutcome,
    pub result: ResultStatus,
    pub usage_source: UsageSource,
    pub usage: UsageReport,
}

impl AttemptReport {
    /// True when the process succeeded and, if one was requested, the typed
    /// result is usable.
    ///
    /// Usage completeness is deliberately not part of this: an attempt that did
    /// the work but reported no tokens succeeded, and the gap belongs in the
    /// usage report rather than in a verdict about the work.
    pub fn succeeded(&self) -> bool {
        self.process.succeeded() && !self.result.is_failure()
    }
}

/// Runs one attempt, then collects its typed result and usage.
///
/// Result and usage collection always run, including after a timeout or
/// cancellation, because a killed attempt still consumed tokens and may have
/// written a partial result. Their failures are reported in the relevant field
/// and never turned into a process failure.
pub async fn run_attempt<C>(spec: &AttemptSpec, cancel: C) -> Result<AttemptReport>
where
    C: Future<Output = ()>,
{
    let occurred_at_unix_ms = unix_millis();
    let process = run_worker(&spec.worker, cancel).await?;
    let result = result::load(spec.result_file.as_deref(), spec.result_schema.as_deref());
    let usage = collect_usage(spec, &process);

    Ok(AttemptReport {
        run_id: spec.run_id.clone(),
        attempt_id: spec.attempt_id.clone(),
        client_event_id: spec.client_event_id(),
        role: spec.role,
        occurred_at_unix_ms,
        process,
        result,
        usage_source: spec.usage_source.clone(),
        usage,
    })
}

fn collect_usage(spec: &AttemptSpec, process: &WorkerOutcome) -> UsageReport {
    let (path, truncated) = match &spec.usage_source {
        UsageSource::None => return usage::unread(spec.accumulation),
        UsageSource::Stdout => (process.stdout.path.as_path(), process.stdout.truncated),
        UsageSource::Stderr => (process.stderr.path.as_path(), process.stderr.truncated),
        UsageSource::File { path } => (path.as_path(), false),
    };

    let mut report = match read_usage(path, spec.accumulation) {
        Ok(report) => report,
        Err(error) => {
            let mut normalizer = usage::UsageNormalizer::new(spec.accumulation);
            normalizer.note(UsageNote::StreamUnreadable {
                reason: format!("{}: {error}", path.display()),
            });
            normalizer.finish()
        }
    };
    if truncated {
        report.notes.push(UsageNote::StreamTruncated);
        // Events past the cap were never written, so whatever was recovered is
        // at best a lower bound.
        if report.status == UsageStatus::Complete {
            report.status = UsageStatus::Partial;
        }
    }
    report
}

fn read_usage(path: &Path, accumulation: Accumulation) -> std::io::Result<UsageReport> {
    let file = File::open(path)?;
    codex::parse_stream(BufReader::new(file), accumulation)
}

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}
