//! Taking, inspecting or throwing away what a run produced.
//!
//! These touch a real repository because the whole point is the git behaviour.
//! The properties that matter: a dirty tree is never merged into, `discard`
//! deletes only the named run's branches, and a run with several branches and
//! no integrated result refuses rather than guessing which one to merge.

use std::path::Path;

use timon::run::record::{Base, Run, Status};
use timon::run::review::{ReviewError, accept, diff, discard, produced};

fn git(repo: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim_end().to_string()
}

/// A repository with one commit, and a helper to add a run's branch to it.
fn repo() -> (tempfile::TempDir, std::path::PathBuf, String) {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("project");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.email", "d@e"]);
    git(&repo, &["config", "user.name", "D"]);
    std::fs::write(repo.join("README.md"), "# project\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "initial"]);
    let head = git(&repo, &["rev-parse", "HEAD"]);
    (dir, repo, head)
}

fn a_run(id: &str, repo: &Path, base: &str) -> Run {
    Run {
        id: id.to_string(),
        principal_uid: 1000,
        goal: "do the thing".to_string(),
        workspace: Some(repo.to_path_buf()),
        base: Base::Head {
            commit: base.to_string(),
        },
        accounts: Vec::new(),
        max_attempts: 8,
        token_ceiling: None,
        deadline: None,
        status: Status::Finished,
        started_at: 0,
        ended_at: Some(1),
        detail: None,
        submission_key: None,
    }
}

/// Puts a commit on a branch named like one of the run's, without touching the
/// checked-out tree.
fn branch_with_file(repo: &Path, branch: &str, path: &str, body: &str) {
    let head = git(repo, &["rev-parse", "HEAD"]);
    git(repo, &["checkout", "-q", "-b", branch]);
    if let Some(parent) = Path::new(path).parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(repo.join(parent)).unwrap();
    }
    std::fs::write(repo.join(path), body).unwrap();
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-qm", &format!("add {path}")]);
    git(repo, &["checkout", "-q", &head]);
    // Back on a detached head; put the branch pointer back where tests expect.
    git(repo, &["checkout", "-q", "-B", "main", &head]);
}

#[test]
fn the_integrated_result_is_preferred_over_the_worker_branches() {
    let (_dir, repo, head) = repo();
    let run = a_run("run-1", &repo, &head);
    branch_with_file(&repo, "timon/run-1/write-a", "a.md", "a\n");
    branch_with_file(&repo, "timon/run-1/write-b", "b.md", "b\n");
    branch_with_file(&repo, "timon/run-1/result", "a.md", "a\n");

    let found = produced(&run).expect("a branch");
    assert_eq!(
        found.branch, "timon/run-1/result",
        "the result branch is what the host merged the workers into"
    );
}

#[test]
fn a_single_worker_branch_is_used_when_there_is_no_result() {
    let (_dir, repo, head) = repo();
    let run = a_run("run-2", &repo, &head);
    branch_with_file(&repo, "timon/run-2/cheap_worker", "notes.md", "hi\n");

    assert_eq!(produced(&run).unwrap().branch, "timon/run-2/cheap_worker");
}

/// `prepared/` branches are the trees handed to dependent tasks, not anyone's
/// work. Counting one would make a perfectly ordinary run look ambiguous.
#[test]
fn prepared_branches_are_not_candidates() {
    let (_dir, repo, head) = repo();
    let run = a_run("run-3", &repo, &head);
    branch_with_file(&repo, "timon/run-3/write-a", "a.md", "a\n");
    branch_with_file(&repo, "timon/run-3/prepared/write-b", "a.md", "a\n");

    assert_eq!(produced(&run).unwrap().branch, "timon/run-3/write-a");
}

#[test]
fn several_branches_with_no_result_refuse_rather_than_guess() {
    let (_dir, repo, head) = repo();
    let run = a_run("run-4", &repo, &head);
    branch_with_file(&repo, "timon/run-4/write-a", "a.md", "a\n");
    branch_with_file(&repo, "timon/run-4/write-b", "b.md", "b\n");

    match produced(&run) {
        Err(ReviewError::Ambiguous(branches)) => assert_eq!(branches.len(), 2),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn a_run_with_no_branch_says_so() {
    let (_dir, repo, head) = repo();
    let run = a_run("run-5", &repo, &head);
    assert!(matches!(produced(&run), Err(ReviewError::NothingProduced)));
}

#[test]
fn a_diff_is_taken_against_where_the_run_started() {
    let (_dir, repo, head) = repo();
    let run = a_run("run-6", &repo, &head);
    branch_with_file(&repo, "timon/run-6/result", "new.md", "new\n");

    // The developer's own work lands after the run started. It must not appear
    // in the run's diff.
    std::fs::write(repo.join("mine.md"), "mine\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "my own work"]);

    let found = produced(&run).unwrap();
    let shown = diff(&found, true).unwrap();
    assert!(
        shown.contains("new.md"),
        "the run's file is missing: {shown}"
    );
    assert!(
        !shown.contains("mine.md"),
        "the developer's own commit was attributed to the run: {shown}"
    );
}

/// The property most likely to cost somebody work.
#[test]
fn a_dirty_tree_is_never_merged_into() {
    let (_dir, repo, head) = repo();
    let run = a_run("run-7", &repo, &head);
    branch_with_file(&repo, "timon/run-7/result", "new.md", "new\n");
    std::fs::write(repo.join("README.md"), "# edited, uncommitted\n").unwrap();

    match accept(&produced(&run).unwrap()) {
        Err(ReviewError::WorkspaceDirty(detail)) => {
            assert!(detail.contains("README.md"), "got {detail}");
        }
        other => panic!("a dirty tree must refuse the merge, got {other:?}"),
    }
    // And the edit is still there, untouched.
    let readme = std::fs::read_to_string(repo.join("README.md")).unwrap();
    assert_eq!(readme, "# edited, uncommitted\n");
}

#[test]
fn accepting_merges_the_work_into_the_current_branch() {
    let (_dir, repo, head) = repo();
    let run = a_run("run-8", &repo, &head);
    branch_with_file(&repo, "timon/run-8/result", "new.md", "new\n");

    accept(&produced(&run).unwrap()).expect("the merge");
    assert!(
        repo.join("new.md").exists(),
        "the run's file is not in the working tree after accepting"
    );
    // --no-ff, so the merge is one commit that can be reverted as one thing.
    let subject = git(&repo, &["log", "-1", "--format=%s"]);
    assert!(subject.starts_with("Merge Timon run"), "got {subject}");
    assert_eq!(
        git(&repo, &["rev-list", "--count", "--merges", "HEAD"]),
        "1"
    );
}

/// Discard must delete this run's branches and nothing else — not another
/// run's, and not the developer's own.
#[test]
fn discarding_removes_only_that_runs_branches() {
    let (_dir, repo, head) = repo();
    let mine = a_run("run-9", &repo, &head);
    branch_with_file(&repo, "timon/run-9/result", "a.md", "a\n");
    branch_with_file(&repo, "timon/run-9/write-a", "a.md", "a\n");
    branch_with_file(&repo, "timon/run-10/result", "b.md", "b\n");
    branch_with_file(&repo, "my-own-feature", "c.md", "c\n");

    let deleted = discard(&mine).expect("discarded");
    assert_eq!(deleted.len(), 2, "deleted: {deleted:?}");

    let left = git(&repo, &["branch", "--format=%(refname:short)"]);
    assert!(!left.contains("timon/run-9/"), "left: {left}");
    assert!(
        left.contains("timon/run-10/result"),
        "another run's branch was deleted: {left}"
    );
    assert!(left.contains("my-own-feature"), "left: {left}");
}

#[test]
fn discarding_twice_says_there_is_nothing_left() {
    let (_dir, repo, head) = repo();
    let run = a_run("run-11", &repo, &head);
    branch_with_file(&repo, "timon/run-11/result", "a.md", "a\n");
    discard(&run).expect("first");
    assert!(matches!(discard(&run), Err(ReviewError::NothingProduced)));
}

#[test]
fn a_run_without_a_workspace_cannot_be_reviewed() {
    let (_dir, repo, head) = repo();
    let mut run = a_run("run-12", &repo, &head);
    run.workspace = None;
    assert!(matches!(produced(&run), Err(ReviewError::NoWorkspace)));
    assert!(matches!(discard(&run), Err(ReviewError::NoWorkspace)));
}

/// Discard must survive the worktrees the run itself left behind.
///
/// Found by discarding a real run: the worker worktrees still had the branches
/// checked out, and git refuses to delete a branch a worktree holds —
/// `cannot delete branch '…' used by worktree at '…'`. So discard removed
/// nothing and reported a git error.
#[test]
fn discarding_removes_the_runs_own_worktrees_first() {
    let (dir, repo, head) = repo();
    let run = a_run("run-13", &repo, &head);
    branch_with_file(&repo, "timon/run-13/write-a", "a.md", "a\n");

    // What the executor does: a worktree per worker, holding its branch.
    let held = dir.path().join("worktrees").join("write-a");
    git(
        &repo,
        &[
            "worktree",
            "add",
            held.to_str().unwrap(),
            "timon/run-13/write-a",
        ],
    );
    assert!(held.join("a.md").exists(), "the worktree was not created");

    let deleted = discard(&run).expect("discard must not be defeated by its own worktrees");
    assert_eq!(deleted, vec!["timon/run-13/write-a".to_string()]);
    let left = git(&repo, &["branch", "--format=%(refname:short)"]);
    assert!(!left.contains("timon/run-13"), "left: {left}");
}

/// Only this run's worktrees go. A developer's own worktree is matched on the
/// branch it holds, not on where it happens to live.
#[test]
fn discarding_leaves_a_developers_own_worktree_alone() {
    let (dir, repo, head) = repo();
    let run = a_run("run-14", &repo, &head);
    branch_with_file(&repo, "timon/run-14/result", "a.md", "a\n");
    branch_with_file(&repo, "my-feature", "b.md", "b\n");

    let mine = dir.path().join("my-checkout");
    git(
        &repo,
        &["worktree", "add", mine.to_str().unwrap(), "my-feature"],
    );

    discard(&run).expect("discarded");
    assert!(
        mine.join("b.md").exists(),
        "the developer's own worktree was removed"
    );
    let left = git(&repo, &["branch", "--format=%(refname:short)"]);
    assert!(left.contains("my-feature"), "left: {left}");
}

/// A discard that cannot finish says which half went.
///
/// Deleting branches is not one transaction. The first version returned on the
/// first failure, so a real discard deleted the result branch, hit a branch a
/// worktree still held, and reported only the error — leaving the developer no
/// way to know half their run was already gone.
///
/// The message is what is tested here. Reproducing an undeletable branch means
/// contriving a git state that git then declines to keep in place, and a test
/// that fakes it badly would be worse than one that says plainly it is checking
/// the report rather than the condition.
#[test]
fn a_partial_discard_names_what_went_and_what_stayed() {
    let shown = ReviewError::PartlyDiscarded {
        deleted: vec!["timon/run-15/result".to_string()],
        refused: vec![(
            "timon/run-15/write-a".to_string(),
            "used by worktree at '/…/worktrees/write-a'".to_string(),
        )],
    }
    .to_string();

    assert!(
        shown.contains("only part of that run was discarded"),
        "got {shown}"
    );
    assert!(shown.contains("gone  timon/run-15/result"), "got {shown}");
    assert!(shown.contains("kept  timon/run-15/write-a"), "got {shown}");
    assert!(
        shown.contains("used by worktree"),
        "the reason a branch stayed is what tells the developer what to clear: {shown}"
    );
    assert!(
        shown.contains("again"),
        "it should say the discard is repeatable: {shown}"
    );
}
