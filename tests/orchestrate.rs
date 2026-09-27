//! Plan, delegate, integrate — and who decides what.
//!
//! The lead is stood in for by a script, so these check the host's behaviour:
//! that it runs what the lead asked for, unedited, and reports honestly when it
//! cannot. Whether a real lead plans *well* is an evaluation question, not
//! something a test can settle.

use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;

use timon::admission::{Ledger, RunLimits};
use timon::attempt::UsageSource;
use timon::orchestrate::{Phases, run};
use timon::usage::Accumulation;

/// A lead that emits whatever plan it is given, then reports what it received.
fn fake_lead(dir: &Path, plan: &str) -> OsString {
    let path = dir.join("lead.sh");
    std::fs::write(
        &path,
        format!(
            r#"#!/bin/sh
prompt=$(cat)
if echo "$prompt" | grep -q 'Reply with JSON only'; then
  cat > "$1" <<'PLAN'
{plan}
PLAN
else
  printf '{{"answer":"saw %s usable","seen":"%s"}}' \
    "$(echo "$prompt" | grep -c 'returned a result matching')" \
    "$(echo "$prompt" | grep -c 'you asked:')" > "$1"
fi
echo '{{"type":"turn.completed","usage":{{"input_tokens":1000,"output_tokens":100}}}}'
"#
        ),
    )
    .unwrap();
    make_executable(&path);
    OsString::from(path)
}

/// A worker that echoes its task back, or fails when told to.
fn fake_worker(dir: &Path, fail: bool) -> OsString {
    let path = dir.join("worker.sh");
    let body = if fail {
        "#!/bin/sh\ncat > /dev/null\nexit 3\n".to_string()
    } else {
        r#"#!/bin/sh
task=$(cat)
printf '{"answer":"%s"}' "$(echo "$task" | tr -d '"' | head -c 60)" > "$1"
echo '{"type":"turn.completed","usage":{"input_tokens":200,"output_tokens":20}}'
"#
        .to_string()
    };
    std::fs::write(&path, body).unwrap();
    make_executable(&path);
    OsString::from(path)
}

fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}

fn schema(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("deliverable.json");
    std::fs::write(
        &path,
        r#"{"type":"object","required":["answer"],"properties":{"answer":{"type":"string"},
            "seen":{"type":"string"}},"additionalProperties":false}"#,
    )
    .unwrap();
    path
}

fn phases(dir: &Path, plan: &str, fail_workers: bool, ledger: Option<Ledger>) -> Phases {
    Phases {
        run_id: "t".to_string(),
        goal: "compare two things".to_string(),
        lead_command: vec![fake_lead(dir, plan), OsString::from("{result}")],
        worker_command: vec![fake_worker(dir, fail_workers), OsString::from("{result}")],
        output_root: dir.join("run"),
        lead_deadline: Duration::from_secs(20),
        worker_deadline: Duration::from_secs(20),
        max_tasks: 4,
        max_task_bytes: 65_536,
        max_output_bytes: 65_536,
        slots: None,
        ledger,
        limits: RunLimits {
            max_attempts: 64,
            token_ceiling: None,
            attempt_reserve: 5_000,
        },
        usage_source: UsageSource::Stdout,
        accumulation: Accumulation::PerTurnDelta,
        deliverable_schema: Some(schema(dir)),
    }
}

const TWO_TASKS: &str = r#"{"tasks":[{"label":"alpha","task":"Find alpha"},
{"label":"beta","task":"Find beta"}],"notes":"independent"}"#;

#[tokio::test]
async fn a_plan_is_delegated_and_the_results_come_back_to_the_lead() {
    let dir = tempfile::tempdir().unwrap();
    let outcome = run(&phases(dir.path(), TWO_TASKS, false, None))
        .await
        .unwrap();

    assert_eq!(outcome.delegated.len(), 2);
    assert!(outcome.delegated.iter().all(|d| d.result.is_some()));
    // The lead's own answer reports how many usable results it was shown, which
    // is how we know the results actually reached it rather than being counted
    // here and summarised away.
    assert!(
        outcome.answer.as_deref().unwrap().contains("saw 2 usable"),
        "the lead should have been shown both results, got {:?}",
        outcome.answer
    );
    assert_eq!(outcome.stopped, None);
}

#[tokio::test]
async fn the_host_passes_on_the_task_the_lead_wrote_without_touching_it() {
    // The host sequences; it does not edit. A task quietly reworded here would
    // move authority out of the lead and into this file.
    let dir = tempfile::tempdir().unwrap();
    let plan = r#"{"tasks":[{"label":"odd","task":"Find X, and mind the  spacing + punctuation!"}],
"notes":"n"}"#;
    let outcome = run(&phases(dir.path(), plan, false, None)).await.unwrap();

    assert_eq!(
        outcome.delegated[0].task,
        "Find X, and mind the  spacing + punctuation!"
    );
}

#[tokio::test]
async fn a_plan_that_breaks_its_contract_delegates_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let outcome = run(&phases(
        dir.path(),
        r#"{"tasks":"not an array"}"#,
        false,
        None,
    ))
    .await
    .unwrap();

    assert!(outcome.delegated.is_empty());
    assert!(outcome.plan.is_none());
    assert!(outcome.stopped.unwrap().contains("no usable plan"));
}

#[tokio::test]
async fn an_empty_plan_is_respected_rather_than_second_guessed() {
    // A lead saying "this needs no delegation" is a decision, not a failure.
    let dir = tempfile::tempdir().unwrap();
    let outcome = run(&phases(
        dir.path(),
        r#"{"tasks":[],"notes":"I can answer this myself"}"#,
        false,
        None,
    ))
    .await
    .unwrap();

    assert!(outcome.delegated.is_empty());
    assert_eq!(outcome.plan.unwrap().notes, "I can answer this myself");
    assert!(
        outcome.answer.is_some(),
        "the lead still produces the answer"
    );
    assert_eq!(outcome.stopped, None);
}

#[tokio::test]
async fn a_failed_worker_still_reaches_the_lead_as_a_failure() {
    // Hiding it would leave the lead to invent what the worker did not return.
    let dir = tempfile::tempdir().unwrap();
    let outcome = run(&phases(dir.path(), TWO_TASKS, true, None))
        .await
        .unwrap();

    assert_eq!(outcome.delegated.len(), 2);
    assert!(outcome.delegated.iter().all(|d| d.result.is_none()));
    assert!(
        outcome
            .delegated
            .iter()
            .all(|d| d.status.contains("failed"))
    );
    assert!(
        outcome.answer.as_deref().unwrap().contains("saw 0 usable"),
        "the lead must be told nothing usable came back"
    );
}

#[tokio::test]
async fn more_tasks_than_allowed_are_cut_and_the_cut_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    let plan = r#"{"tasks":[{"label":"a","task":"1"},{"label":"b","task":"2"},
{"label":"c","task":"3"}],"notes":"three"}"#;
    let mut phases = phases(dir.path(), plan, false, None);
    phases.max_tasks = 2;

    let outcome = run(&phases).await.unwrap();

    assert_eq!(outcome.delegated.len(), 2);
    assert_eq!(
        outcome.tasks_dropped_over_limit, 1,
        "a silent cut would leave the lead integrating results for work it thinks it asked for"
    );
}

#[tokio::test]
async fn the_lead_is_charged_to_the_run_as_well_as_the_workers() {
    // The lead is usually the expensive part. A ceiling covering only workers
    // would leave the dominant cost unbounded while looking like a limit.
    let dir = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(&dir.path().join("led"), "t").unwrap();
    let outcome = run(&phases(dir.path(), TWO_TASKS, false, Some(ledger)))
        .await
        .unwrap();

    let reopened = Ledger::open(&dir.path().join("led"), "t").unwrap();
    let summary = reopened.summary().unwrap();
    assert_eq!(
        summary.attempts, 4,
        "two lead phases and two workers, all four accounted"
    );
    assert!(outcome.lead_tokens.unwrap() > outcome.worker_tokens.unwrap());
}

#[tokio::test]
async fn an_allowance_too_small_to_delegate_still_asks_the_lead_what_is_missing() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(&dir.path().join("led"), "t").unwrap();
    let mut phases = phases(dir.path(), TWO_TASKS, false, Some(ledger));
    // Enough for planning, not enough for a worker on top.
    phases.limits.token_ceiling = Some(6_000);

    let outcome = run(&phases).await.unwrap();

    assert!(outcome.delegated.is_empty());
    assert!(
        outcome.stopped.as_deref().unwrap().contains("ceiling"),
        "the reason has to name the limit that stopped it: {:?}",
        outcome.stopped
    );
}

#[tokio::test]
async fn token_totals_are_absent_rather_than_zero_when_nothing_reported_any() {
    let dir = tempfile::tempdir().unwrap();
    let mut phases = phases(dir.path(), TWO_TASKS, false, None);
    // Nothing is read, so no usage is ever reported.
    phases.usage_source = UsageSource::None;

    let outcome = run(&phases).await.unwrap();

    assert_eq!(
        outcome.lead_tokens, None,
        "no reports must not read as a genuine zero"
    );
    assert_eq!(outcome.worker_tokens, None);
    assert!(outcome.basis.contains("lower bound"));
}

#[tokio::test]
async fn the_planning_instructions_tell_the_lead_what_a_worker_cannot_see() {
    // Every other rule follows from this one. A lead that misses it writes tasks
    // referring to context the worker will never have.
    let dir = tempfile::tempdir().unwrap();
    let mut phases = phases(dir.path(), TWO_TASKS, false, None);
    // A lead that just copies its prompt into the plan field lets us inspect it.
    phases.lead_command = vec![
        {
            let path = dir.path().join("echo-lead.sh");
            std::fs::write(
                &path,
                r#"#!/bin/sh
prompt=$(cat)
printf '%s' "$prompt" > "$(dirname "$1")/prompt.txt"
printf '{"tasks":[],"notes":"n"}' > "$1"
"#,
            )
            .unwrap();
            make_executable(&path);
            OsString::from(path)
        },
        OsString::from("{result}"),
    ];

    run(&phases).await.unwrap();

    let prompt = std::fs::read_to_string(dir.path().join("run/lead-plan/prompt.txt")).unwrap();
    assert!(prompt.contains("starts fresh"));
    assert!(prompt.contains("not the other workers"));
    assert!(prompt.contains("stand completely on its own"));
    assert!(
        prompt.contains("called again"),
        "the lead has to know it will integrate the results itself"
    );
}
