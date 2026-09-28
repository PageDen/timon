//! The planner route, joined up.
//!
//! Most of these are about refusing badly rather than running well: a planner
//! is a model, and the interesting cases are the ones where it answers with
//! something other than a graph.

use timon::dag::{Access, Limits};
use timon::run::pipeline::{PipelineError, accept, parse_plan, plan_prompt};

#[test]
fn a_plan_wrapped_in_prose_is_still_a_plan() {
    // A model that explains itself before answering has made a formatting slip,
    // not refused to plan. A round trip to correct it costs a model call.
    let answer = r#"Here is how I would split it:

```json
{"tasks":[{"label":"api","task":"add the endpoint","depends_on":[],"access":"write"}],"notes":"one piece"}
```

That should do it."#;
    let plan = parse_plan(answer).expect("a plan");
    assert_eq!(plan.tasks.len(), 1);
    assert_eq!(plan.tasks[0].label, "api");
    assert_eq!(plan.tasks[0].access, Access::Write);
}

#[test]
fn a_plan_with_braces_inside_its_strings_is_read_correctly() {
    // Scanning for the last `}` would truncate this. The task text is the
    // planner's own words and often contains code.
    let answer =
        r#"{"tasks":[{"label":"fmt","task":"replace {} with {:?}","depends_on":[]}],"notes":"}"}"#;
    let plan = parse_plan(answer).expect("a plan");
    assert_eq!(plan.tasks[0].task, "replace {} with {:?}");
    assert_eq!(plan.notes, "}");
}

#[test]
fn an_answer_that_is_not_a_plan_says_what_it_saw() {
    // So a person reading the failure knows whether the model refused, rambled,
    // or answered the question directly.
    let error = parse_plan("I cannot split this goal into independent parts.").unwrap_err();
    match &error {
        PipelineError::NotAPlan { saw, .. } => {
            assert!(saw.contains("cannot split"), "saw: {saw}")
        }
        other => panic!("expected NotAPlan, got {other:?}"),
    }
    assert!(format!("{error}").contains("did not return a task graph"));
}

#[test]
fn a_truncated_plan_is_reported_rather_than_half_read() {
    let error = parse_plan(r#"{"tasks":[{"label":"a","task":"do a""#).unwrap_err();
    assert!(matches!(error, PipelineError::NotAPlan { .. }));
}

#[test]
fn every_fault_in_a_bad_plan_comes_back_at_once() {
    // One round trip fixes all of them. A planner given faults one at a time
    // needs a model call per fault.
    let plan = parse_plan(
        r#"{"tasks":[
             {"label":"a","task":"","depends_on":[]},
             {"label":"a","task":"do it","depends_on":["ghost"]}
           ],"notes":""}"#,
    )
    .unwrap();

    let error = accept(&plan, &Limits::default()).unwrap_err();
    match error {
        PipelineError::InvalidPlan { faults } => {
            assert!(faults.len() >= 3, "{faults:?}");
            let all = faults.join(" ");
            assert!(all.contains("says nothing for a worker to do"));
            assert!(all.contains("two tasks are labelled"));
            assert!(all.contains("not a task in this plan"));
        }
        other => panic!("expected InvalidPlan, got {other:?}"),
    }
}

#[test]
fn a_good_plan_is_accepted_and_ordered() {
    let plan = parse_plan(
        r#"{"tasks":[
             {"label":"screen","task":"build it","depends_on":["api"],"access":"write"},
             {"label":"api","task":"add the endpoint","depends_on":[],"access":"write"}
           ],"notes":"the screen needs the endpoint"}"#,
    )
    .unwrap();
    let graph = accept(&plan, &Limits::default()).expect("accepted");
    assert_eq!(graph.order(), ["api", "screen"]);
    assert_eq!(graph.notes, "the screen needs the endpoint");
}

#[test]
fn the_planner_is_told_what_a_dependency_means_here() {
    // A planner that thinks depends_on is only ordering writes graphs whose
    // dependants cannot see what they depend on, which is the mistake the host
    // spent P3 fixing.
    let prompt = plan_prompt("do the thing", &Limits::default());
    assert!(prompt.contains("not just \nordering") || prompt.contains("not just ordering"));
    assert!(
        prompt.contains("what its dependencies actually"),
        "it should say the host supplies the output, not a description"
    );
    // And what the host will not do about conflicts.
    assert!(prompt.contains("the host will not choose between them"));
    // Limits are stated, since a planner cannot feel them.
    assert!(prompt.contains(&Limits::default().max_tasks.to_string()));
}

#[test]
fn the_planner_prompt_carries_no_stray_characters() {
    // This caught a CJK character that had been typed into the prompt by
    // accident. It checks for characters from a script the prompt has no
    // business containing, rather than for non-ascii — an em dash is
    // punctuation, not a slip, and a test that bans it would just get deleted.
    let prompt = plan_prompt("goal", &Limits::default());
    let stray: Vec<char> = prompt
        .chars()
        .filter(|c| {
            let n = *c as u32;
            // CJK, Hiragana, Katakana, Hangul, Cyrillic, Arabic, Hebrew.
            (0x0400..=0x05FF).contains(&n)
                || (0x0600..=0x06FF).contains(&n)
                || (0x3040..=0x30FF).contains(&n)
                || (0x3400..=0x9FFF).contains(&n)
                || (0xAC00..=0xD7AF).contains(&n)
        })
        .collect();
    assert!(
        stray.is_empty(),
        "stray characters in the prompt: {stray:?}"
    );
    assert!(prompt.contains("Reply with JSON only"));
}

#[test]
fn a_writing_task_that_wrote_nothing_is_a_failure_not_a_success() {
    // Found the first time the planner route ran end to end. Three tasks
    // reported done and the result branch contained nothing: the sandbox had
    // refused the writes and each worker explained, at length and with exit
    // code 0, what it would have written.
    use timon::run::execute::changed_nothing;

    let said = "I couldn't create NOTES.md because the workspace is read-only.\n\
                Its intended content is:\n# Notes";
    let complaint = changed_nothing(Access::Write, true, said).expect("a failure");
    assert!(complaint.contains("changed none"));
    assert!(
        complaint.contains("read-only"),
        "it carries what the worker said, which named the cause: {complaint}"
    );

    // A writing task that wrote is fine.
    assert!(changed_nothing(Access::Write, false, "done").is_none());
    // And a reading task is not expected to write at all.
    assert!(changed_nothing(Access::Read, true, "here is the answer").is_none());
}
