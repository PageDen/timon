//! Where a writing worker works, and what the host does with what it leaves.
//!
//! Real repositories in temporary directories: git's behaviour is the thing
//! under test, and a fake would be testing the fake. Nothing here calls a model.

use timon::worktree::{GitError, Workspace};

struct Repo {
    dir: tempfile::TempDir,
}

impl Repo {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo { dir };
        repo.git(&["init", "--quiet", "-b", "main", "."]);
        repo.git(&["config", "user.email", "test@localhost"]);
        repo.git(&["config", "user.name", "test"]);
        repo.write("seed.txt", "seed\n");
        repo.git(&["add", "-A"]);
        repo.git(&["commit", "--quiet", "-m", "seed"]);
        repo
    }

    fn path(&self) -> &std::path::Path {
        self.dir.path()
    }

    fn git(&self, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(self.path())
            .args(args)
            .output()
            .expect("git runs");
        String::from_utf8_lossy(&out.stdout).to_string()
    }

    fn write(&self, name: &str, content: &str) {
        std::fs::write(self.path().join(name), content).unwrap();
    }

    fn head(&self) -> String {
        self.git(&["rev-parse", "HEAD"]).trim().to_string()
    }
}

fn workspace(repo: &Repo, root: &std::path::Path) -> Workspace {
    Workspace::open(repo.path(), "run-1", &repo.head(), root).expect("opens")
}

#[test]
fn each_worker_gets_its_own_worktree_and_branch() {
    let repo = Repo::new();
    let root = tempfile::tempdir().unwrap();
    let mut workspace = workspace(&repo, root.path());

    let one = workspace.worktree_for("api").unwrap();
    let two = workspace.worktree_for("screen").unwrap();

    assert_ne!(one.path, two.path, "workers never share a directory");
    assert_ne!(one.branch, two.branch);
    assert!(one.path.join("seed.txt").exists(), "branched from the base");
    assert!(
        one.branch.contains("run-1"),
        "namespaced by run: {}",
        one.branch
    );
}

#[test]
fn two_workers_editing_the_same_file_produce_two_branches_not_one_mess() {
    // What the isolation is actually for.
    let repo = Repo::new();
    let root = tempfile::tempdir().unwrap();
    let mut workspace = workspace(&repo, root.path());

    let a = workspace.worktree_for("a").unwrap();
    let b = workspace.worktree_for("b").unwrap();
    std::fs::write(a.path.join("seed.txt"), "from a\n").unwrap();
    std::fs::write(b.path.join("seed.txt"), "from b\n").unwrap();

    assert!(workspace.commit_work(&a).unwrap().is_some());
    assert!(workspace.commit_work(&b).unwrap().is_some());

    // Neither touched the other, and neither touched the developer's tree.
    assert_eq!(
        std::fs::read_to_string(a.path.join("seed.txt")).unwrap(),
        "from a\n"
    );
    assert_eq!(
        std::fs::read_to_string(b.path.join("seed.txt")).unwrap(),
        "from b\n"
    );
    assert_eq!(
        std::fs::read_to_string(repo.path().join("seed.txt")).unwrap(),
        "seed\n",
        "the developer's working tree is untouched"
    );
}

#[test]
fn a_worker_that_changed_nothing_is_not_a_failure() {
    // A task that concluded no change was needed has said something useful.
    let repo = Repo::new();
    let root = tempfile::tempdir().unwrap();
    let mut workspace = workspace(&repo, root.path());
    let idle = workspace.worktree_for("idle").unwrap();
    assert_eq!(workspace.commit_work(&idle).unwrap(), None);
}

#[test]
fn a_dependant_starts_from_a_tree_containing_what_it_depends_on() {
    // The P3 rule, made real: not ordering, but a commit that actually has the
    // predecessor's work in it.
    let repo = Repo::new();
    let root = tempfile::tempdir().unwrap();
    let mut workspace = workspace(&repo, root.path());

    let api = workspace.worktree_for("api").unwrap();
    std::fs::write(api.path.join("api.rs"), "pub fn get_user() {}\n").unwrap();
    workspace.commit_work(&api).unwrap();

    let prepared = workspace.prepare(&["api".to_string()]).unwrap();

    // The prepared commit contains the API.
    let tree = std::process::Command::new("git")
        .arg("-C")
        .arg(repo.path())
        .args(["show", &format!("{prepared}:api.rs")])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&tree.stdout),
        "pub fn get_user() {}\n",
        "the dependant would see the exact interface its dependency defined"
    );
}

#[test]
fn a_conflict_names_the_files_and_is_not_resolved_here() {
    let repo = Repo::new();
    let root = tempfile::tempdir().unwrap();
    let mut workspace = workspace(&repo, root.path());

    let a = workspace.worktree_for("a").unwrap();
    let b = workspace.worktree_for("b").unwrap();
    std::fs::write(a.path.join("seed.txt"), "from a\n").unwrap();
    std::fs::write(b.path.join("seed.txt"), "from b\n").unwrap();
    workspace.commit_work(&a).unwrap();
    workspace.commit_work(&b).unwrap();

    let error = workspace
        .prepare(&["a".to_string(), "b".to_string()])
        .unwrap_err();

    match &error {
        GitError::Conflict { between, detail } => {
            assert_eq!(between, &vec!["a".to_string(), "b".to_string()]);
            assert!(detail.contains("seed.txt"), "names the file: {detail}");
        }
        other => panic!("expected a conflict, got {other:?}"),
    }
    assert!(
        format!("{error}").contains("does not choose a side"),
        "the message says whose decision this is not"
    );
}

#[test]
fn integration_merges_in_dependency_order_onto_one_branch() {
    let repo = Repo::new();
    let root = tempfile::tempdir().unwrap();
    let mut workspace = workspace(&repo, root.path());

    for (label, file) in [("api", "api.rs"), ("screen", "screen.rs")] {
        let tree = workspace.worktree_for(label).unwrap();
        std::fs::write(tree.path.join(file), format!("// {label}\n")).unwrap();
        workspace.commit_work(&tree).unwrap();
    }

    let result = workspace
        .integrate(&["api".to_string(), "screen".to_string()])
        .unwrap();

    let files = std::process::Command::new("git")
        .arg("-C")
        .arg(repo.path())
        .args(["ls-tree", "--name-only", "-r", &result])
        .output()
        .unwrap();
    let listing = String::from_utf8_lossy(&files.stdout);
    assert!(listing.contains("api.rs") && listing.contains("screen.rs"));
    assert!(
        listing.contains("seed.txt"),
        "and the base is still there, so nothing was replaced wholesale"
    );
}

#[test]
fn integration_skips_a_task_that_produced_nothing() {
    let repo = Repo::new();
    let root = tempfile::tempdir().unwrap();
    let mut workspace = workspace(&repo, root.path());

    let api = workspace.worktree_for("api").unwrap();
    std::fs::write(api.path.join("api.rs"), "// api\n").unwrap();
    workspace.commit_work(&api).unwrap();

    // "thought" never got a worktree, so it has no branch at all.
    let result = workspace
        .integrate(&["api".to_string(), "thought".to_string()])
        .unwrap();
    assert!(!result.is_empty());
}

#[test]
fn discarding_removes_this_runs_work_and_nothing_else() {
    let repo = Repo::new();
    let root = tempfile::tempdir().unwrap();

    // Something that belongs to somebody else.
    repo.git(&["branch", "someone-elses-work"]);

    let mut workspace = workspace(&repo, root.path());
    let a = workspace.worktree_for("a").unwrap();
    let path = a.path.clone();
    assert!(path.exists());

    let removed = workspace.discard();

    assert!(!path.exists(), "the worktree is gone");
    assert!(!removed.is_empty(), "and it says what it removed");
    let branches = repo.git(&["branch", "--list"]);
    assert!(
        branches.contains("someone-elses-work"),
        "a cleanup that guesses eventually deletes somebody's work: {branches}"
    );
    assert!(!branches.contains("timon/run-1/a"));
}

#[test]
fn a_failed_run_keeps_its_worktree_until_somebody_asks() {
    // Deleting the evidence the moment a run fails is how a failure becomes
    // unexplainable. Discarding is a method, not a cleanup path nobody chose.
    let repo = Repo::new();
    let root = tempfile::tempdir().unwrap();
    let mut workspace = workspace(&repo, root.path());
    let a = workspace.worktree_for("a").unwrap();
    std::fs::write(a.path.join("half-done.txt"), "partial\n").unwrap();

    // No discard called. The evidence is still there.
    assert!(a.path.join("half-done.txt").exists());
    assert_eq!(workspace.made().len(), 1);
}

#[test]
fn a_label_cannot_escape_through_its_own_name() {
    // A planner chooses labels. One that becomes `../..` or breaks a ref is not
    // something to find out about in production.
    let repo = Repo::new();
    let root = tempfile::tempdir().unwrap();
    let workspace = workspace(&repo, root.path());

    for nasty in ["../../etc", "a/b", "--force", "", "..", "refs/heads/main"] {
        let branch = workspace.branch_for(nasty);
        assert!(!branch.contains(".."), "{nasty:?} -> {branch}");
        assert!(branch.starts_with("timon/run-1/"), "{nasty:?} -> {branch}");
        // And it is a name git will actually accept.
        let ok = std::process::Command::new("git")
            .arg("-C")
            .arg(repo.path())
            .args(["check-ref-format", &format!("refs/heads/{branch}")])
            .status()
            .unwrap();
        assert!(ok.success(), "git rejected {branch:?} from {nasty:?}");
    }
}

#[test]
fn opening_something_that_is_not_a_repository_fails_immediately() {
    // Rather than when the first worker starts, halfway through a run.
    let empty = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let error = Workspace::open(empty.path(), "run-1", "HEAD", root.path()).unwrap_err();
    assert!(matches!(error, GitError::Refused { .. }));
}

/// A runner backed by real worktrees, which is how a writing task runs for real.
struct WorktreeRunner {
    workspace: std::sync::Mutex<Workspace>,
    /// What each task should write, so the test decides the work rather than a
    /// model.
    writes: std::collections::BTreeMap<String, (String, String)>,
}

impl timon::dag_run::Runner for WorktreeRunner {
    fn run(
        &self,
        task: &timon::dag::Task,
        input: &timon::dag_inputs::Input,
        _cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<timon::dag_inputs::Artifact, String> {
        let mut workspace = self.workspace.lock().unwrap();
        let tree = workspace
            .worktree_for(&task.label)
            .map_err(|error| error.to_string())?;

        // A writing task that depends on something starts from the prepared
        // commit, so what it edits already contains its dependency's work.
        if let timon::dag_inputs::Input::PreparedCommit { commit, .. } = input {
            std::process::Command::new("git")
                .arg("-C")
                .arg(&tree.path)
                .args(["reset", "--hard", "--quiet", commit])
                .status()
                .map_err(|e| e.to_string())?;
        }

        if let Some((name, content)) = self.writes.get(&task.label) {
            std::fs::write(tree.path.join(name), content).map_err(|e| e.to_string())?;
        }
        let committed = workspace
            .commit_work(&tree)
            .map_err(|error| error.to_string())?;
        Ok(timon::dag_inputs::Artifact::new(
            &task.label,
            1,
            committed.unwrap_or_else(|| "no change".to_string()),
        ))
    }

    fn prepare(&self, merged: &[String]) -> Result<String, String> {
        self.workspace
            .lock()
            .unwrap()
            .prepare(merged)
            .map_err(|error| error.to_string())
    }
}

#[test]
fn a_writing_graph_runs_in_worktrees_and_a_dependant_sees_its_dependency() {
    // The whole of P3 and P4 together: ordering, inputs, isolation, and a
    // result branch — with no model involved.
    use timon::dag::{Access, Limits, Plan, Task, validate};
    use timon::dag_run::{Bounds, run_graph};

    let repo = Repo::new();
    let root = tempfile::tempdir().unwrap();
    let workspace = workspace(&repo, root.path());

    let writing = |label: &str, deps: &[&str]| Task {
        label: label.to_string(),
        task: format!("write {label}"),
        depends_on: deps.iter().map(|s| s.to_string()).collect(),
        access: Access::Write,
        acceptance: Vec::new(),
    };
    let plan = validate(
        &Plan {
            tasks: vec![writing("api", &[]), writing("screen", &["api"])],
            notes: String::new(),
        },
        &Limits::default(),
    )
    .unwrap();

    let runner = WorktreeRunner {
        workspace: std::sync::Mutex::new(workspace),
        writes: [
            (
                "api".to_string(),
                ("api.rs".to_string(), "pub fn get_user() {}\n".to_string()),
            ),
            (
                "screen".to_string(),
                ("screen.rs".to_string(), "// uses get_user\n".to_string()),
            ),
        ]
        .into_iter()
        .collect(),
    };

    let report = run_graph(
        &plan,
        &runner,
        &Bounds {
            concurrency: 1,
            deadline: None,
            writing_permitted: true,
        },
        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    );
    assert!(report.complete, "{:?}", report.tasks);

    // The dependant's worktree contained the dependency's file, which is the
    // acceptance P3 asked for, now proven through real git rather than in the
    // input type alone.
    let screen_tree = root.path().join("screen");
    assert!(
        screen_tree.join("api.rs").exists(),
        "the dependant started from a tree containing its dependency's work"
    );

    // And the developer's own tree was never touched.
    assert!(!repo.path().join("api.rs").exists());

    let result = runner
        .workspace
        .lock()
        .unwrap()
        .integrate(&["api".to_string(), "screen".to_string()])
        .unwrap();
    let listing = repo.git(&["ls-tree", "--name-only", "-r", &result]);
    assert!(
        listing.contains("api.rs") && listing.contains("screen.rs"),
        "{listing}"
    );
}
