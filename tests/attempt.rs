//! Attempt tests: one supervised child, its typed result and its usage.
//!
//! Every child here is a small `/bin/sh` script that prints a fixture event
//! stream, so no model provider is involved. These tests establish that the
//! lead and the workers share one envelope; they prove nothing about a real
//! harness's sandbox, flags or usage completeness.

use std::path::{Path, PathBuf};
use std::time::Duration;
use timon::attempt::{AttemptSpec, Role, UsageSource, run_attempt};
use timon::result::ResultStatus;
use timon::usage::{Accumulation, TokenCount, UsageNote, UsageStatus};
use timon::worker::{WorkerLimits, WorkerSpec};

fn spec(role: Role, script: &str, output_dir: &Path) -> AttemptSpec {
    AttemptSpec {
        role,
        run_id: "run-1".to_owned(),
        attempt_id: "1".to_owned(),
        client_event_id: None,
        worker: WorkerSpec {
            program: PathBuf::from("/bin/sh"),
            args: vec!["-c".into(), script.into()],
            cwd: None,
            env_set: Vec::new(),
            env_remove: Vec::new(),
            task: "summarise the release notes".to_owned(),
            output_dir: output_dir.to_path_buf(),
            limits: WorkerLimits::with_deadline(Duration::from_secs(10)),
        },
        result_file: None,
        result_schema: None,
        usage_source: UsageSource::None,
        accumulation: Accumulation::PerTurnDelta,
    }
}

fn never() -> std::future::Pending<()> {
    std::future::pending()
}

/// A script that prints a two-turn event stream on stdout.
const EVENT_STREAM: &str = concat!(
    r#"cat >/dev/null; "#,
    r#"echo '{"type":"thread.started","thread_id":"t-1"}'; "#,
    r#"echo '{"type":"turn.started"}'; "#,
    r#"echo '{"type":"turn.completed","usage":{"input_tokens":100,"cached_input_tokens":20,"output_tokens":10}}'; "#,
    r#"echo '{"type":"turn.started"}'; "#,
    r#"echo '{"type":"turn.completed","usage":{"input_tokens":200,"cached_input_tokens":50,"output_tokens":30}}'"#,
);

#[tokio::test]
async fn a_worker_attempt_reports_its_result_and_usage_together() {
    let dir = tempfile::tempdir().expect("temp dir");
    let result_file = dir.path().join("result.json");
    let schema = dir.path().join("schema.json");
    std::fs::write(
        &schema,
        r#"{"type":"object","required":["summary"],"properties":{"summary":{"type":"string"}}}"#,
    )
    .expect("writing the schema cannot fail");

    let script = format!(
        "{EVENT_STREAM}; printf '{{\"summary\":\"two fixes\"}}' > {}",
        result_file.display()
    );
    let mut spec = spec(Role::Worker, &script, &dir.path().join("attempt"));
    spec.result_file = Some(result_file);
    spec.result_schema = Some(schema);
    spec.usage_source = UsageSource::Stdout;

    let report = run_attempt(&spec, never()).await.expect("attempt runs");

    assert!(report.succeeded());
    assert_eq!(report.role, Role::Worker);
    assert_eq!(report.usage.usage.input, TokenCount::Known(300));
    assert_eq!(report.usage.total, TokenCount::Known(340));
    assert_eq!(report.usage.billable_input, TokenCount::Known(230));
    assert_eq!(report.usage.status, UsageStatus::Complete);
    assert!(matches!(
        report.result,
        ResultStatus::Parsed {
            schema_validated: true,
            ..
        }
    ));
}

#[tokio::test]
async fn the_lead_uses_the_same_envelope_as_a_worker() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut lead = spec(Role::Lead, EVENT_STREAM, &dir.path().join("lead"));
    lead.usage_source = UsageSource::Stdout;
    let mut worker = spec(Role::Worker, EVENT_STREAM, &dir.path().join("worker"));
    worker.usage_source = UsageSource::Stdout;

    let lead = run_attempt(&lead, never()).await.expect("lead runs");
    let worker = run_attempt(&worker, never()).await.expect("worker runs");

    // The strong lead is accounted for exactly as the cheap worker is; usage
    // tracking that covered only workers would miss the larger consumer.
    assert_eq!(lead.usage.usage, worker.usage.usage);
    assert_eq!(lead.role, Role::Lead);
    assert_eq!(lead.client_event_id, "lead:run-1:1");
    assert_eq!(worker.client_event_id, "worker:run-1:1");
}

#[tokio::test]
async fn the_event_id_is_derived_from_the_role_and_identifiers_so_a_replay_repeats_it() {
    let dir = tempfile::tempdir().expect("temp dir");
    let first = run_attempt(
        &spec(Role::Worker, "cat >/dev/null", &dir.path().join("a")),
        never(),
    )
    .await
    .expect("attempt runs");
    let second = run_attempt(
        &spec(Role::Worker, "cat >/dev/null", &dir.path().join("b")),
        never(),
    )
    .await
    .expect("attempt runs");

    assert_eq!(first.client_event_id, second.client_event_id);
}

#[tokio::test]
async fn an_explicit_event_id_is_preserved() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut spec = spec(Role::Worker, "cat >/dev/null", &dir.path().join("attempt"));
    spec.client_event_id = Some("kept-from-the-spool".to_owned());

    let report = run_attempt(&spec, never()).await.expect("attempt runs");

    assert_eq!(report.client_event_id, "kept-from-the-spool");
}

#[tokio::test]
async fn a_process_that_exits_zero_without_its_result_has_not_succeeded() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut spec = spec(Role::Worker, "cat >/dev/null", &dir.path().join("attempt"));
    spec.result_file = Some(dir.path().join("result.json"));

    let report = run_attempt(&spec, never()).await.expect("attempt runs");

    assert!(report.process.succeeded());
    assert!(!report.succeeded());
    assert!(matches!(report.result, ResultStatus::Missing { .. }));
}

#[tokio::test]
async fn a_timed_out_attempt_still_accounts_for_the_tokens_it_reported() {
    let dir = tempfile::tempdir().expect("temp dir");
    let script = format!("{EVENT_STREAM}; sleep 30");
    let mut spec = spec(Role::Worker, &script, &dir.path().join("attempt"));
    spec.worker.limits = WorkerLimits::with_deadline(Duration::from_millis(400));
    spec.usage_source = UsageSource::Stdout;

    let report = run_attempt(&spec, never()).await.expect("attempt runs");

    assert!(report.process.timed_out);
    assert!(!report.succeeded());
    // A killed attempt consumed tokens; dropping them would understate usage.
    assert_eq!(report.usage.usage.input, TokenCount::Known(300));
}

#[tokio::test]
async fn usage_is_unknown_rather_than_zero_when_no_stream_is_read() {
    let dir = tempfile::tempdir().expect("temp dir");
    let report = run_attempt(
        &spec(Role::Worker, EVENT_STREAM, &dir.path().join("attempt")),
        never(),
    )
    .await
    .expect("attempt runs");

    assert_eq!(report.usage.status, UsageStatus::Unknown);
    assert_eq!(report.usage.usage.input, TokenCount::Unknown);
    assert_eq!(report.usage_source, UsageSource::None);
}

#[tokio::test]
async fn a_usage_stream_that_hit_the_capture_cap_is_reported_as_partial() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut spec = spec(Role::Worker, EVENT_STREAM, &dir.path().join("attempt"));
    spec.usage_source = UsageSource::Stdout;
    // Enough for the first turn, not the second.
    spec.worker.limits.max_output_bytes = 200;

    let report = run_attempt(&spec, never()).await.expect("attempt runs");

    assert!(report.process.stdout.truncated);
    assert!(
        report
            .usage
            .notes
            .iter()
            .any(|note| matches!(note, UsageNote::StreamTruncated))
    );
    // The first turn survived, so the total is a lower bound rather than the
    // attempt's real usage.
    assert_eq!(report.usage.usage.input, TokenCount::Known(100));
    assert_eq!(report.usage.status, UsageStatus::Partial);
}

#[tokio::test]
async fn a_usage_stream_cut_before_any_turn_is_unknown_rather_than_partial() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut spec = spec(Role::Worker, EVENT_STREAM, &dir.path().join("attempt"));
    spec.usage_source = UsageSource::Stdout;
    spec.worker.limits.max_output_bytes = 60;

    let report = run_attempt(&spec, never()).await.expect("attempt runs");

    // Nothing was recovered, so claiming a partial total would overstate it.
    assert_eq!(report.usage.status, UsageStatus::Unknown);
    assert!(
        report
            .usage
            .notes
            .iter()
            .any(|note| matches!(note, UsageNote::StreamTruncated))
    );
}

#[tokio::test]
async fn a_usage_file_the_child_never_wrote_is_reported_as_unreadable() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut spec = spec(Role::Worker, "cat >/dev/null", &dir.path().join("attempt"));
    spec.usage_source = UsageSource::File {
        path: dir.path().join("usage.jsonl"),
    };

    let report = run_attempt(&spec, never()).await.expect("attempt runs");

    assert_eq!(report.usage.status, UsageStatus::Unknown);
    assert!(
        report
            .usage
            .notes
            .iter()
            .any(|note| matches!(note, UsageNote::StreamUnreadable { .. }))
    );
}

#[tokio::test]
async fn a_usage_stream_on_a_separate_file_is_read_from_there() {
    let dir = tempfile::tempdir().expect("temp dir");
    let usage_file = dir.path().join("usage.jsonl");
    let script = format!(
        "cat >/dev/null; printf '%s\\n' '{{\"type\":\"turn.completed\",\"usage\":{{\"input_tokens\":7,\"cached_input_tokens\":0,\"output_tokens\":3}}}}' > {}",
        usage_file.display()
    );
    let mut spec = spec(Role::Worker, &script, &dir.path().join("attempt"));
    spec.usage_source = UsageSource::File {
        path: usage_file.clone(),
    };

    let report = run_attempt(&spec, never()).await.expect("attempt runs");

    assert_eq!(report.usage.total, TokenCount::Known(10));
    assert_eq!(report.usage_source, UsageSource::File { path: usage_file });
}

#[tokio::test]
async fn the_report_serializes_with_the_fields_downstream_usage_tracking_needs() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut spec = spec(Role::Worker, EVENT_STREAM, &dir.path().join("attempt"));
    spec.usage_source = UsageSource::Stdout;

    let report = run_attempt(&spec, never()).await.expect("attempt runs");
    let json = serde_json::to_value(&report).expect("report serializes");

    assert_eq!(json["run_id"], "run-1");
    assert_eq!(json["attempt_id"], "1");
    assert_eq!(json["client_event_id"], "worker:run-1:1");
    assert_eq!(json["role"], "worker");
    assert!(
        json["occurred_at_unix_ms"]
            .as_u64()
            .is_some_and(|ms| ms > 0)
    );
    assert_eq!(json["usage"]["accumulation"], "per_turn_delta");
    assert_eq!(json["usage"]["authority"], "turns");
    assert_eq!(json["result"]["status"], "not_requested");
}
