// Adapted for Timon. Not derived from Prodex source.
//! Where a writing worker works.
//!
//! One worktree per worker, on its own branch, created by the host from the
//! commit the hand-off started at. Workers never share a working directory, so
//! they cannot overwrite each other — and because the host creates the branch,
//! a worker never has to be trusted not to move a ref. The write-sandbox
//! qualification confirmed it cannot: `git branch` and `git update-ref` are both
//! refused inside the sandbox.
//!
//! **A worktree is a workflow boundary, not a security boundary**, and this
//! module does not pretend otherwise. Worktrees of one repository share `.git`.
//! What keeps a worker inside its own is the sandbox, qualified separately in
//! P4.3; what this provides is that two workers editing the same file produce
//! two branches rather than one mess.
//!
//! **Integration is mechanical.** The host merges in dependency order and does
//! not choose a side. A conflict stops the merge and is returned with its
//! detail, to become a verifier finding — resolving it here would be the host
//! deciding what the work should say.
//!
//! **Discarding is explicit.** A failed run keeps its worktrees until somebody
//! asks for them to go, because deleting the evidence the moment a run fails is
//! how a failure becomes unexplainable. P4.3 asked for that and it is a method
//! here rather than a cleanup path nobody chose.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Why a git operation could not be done.
#[derive(Debug)]
pub enum GitError {
    /// Git was not runnable at all.
    Unavailable(String),
    /// Git ran and refused, with what it said.
    Refused { what: String, detail: String },
    /// A merge stopped on conflicting changes. Not an error to recover from
    /// here: it is a finding about the work.
    Conflict {
        between: Vec<String>,
        detail: String,
    },
}

impl std::fmt::Display for GitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GitError::Unavailable(why) => write!(f, "git could not be run: {why}"),
            GitError::Refused { what, detail } => write!(f, "{what}: {detail}"),
            GitError::Conflict { between, detail } => write!(
                f,
                "merging {} stopped on conflicting changes: {detail}. The host \
                 merges and does not choose a side, so this is a finding for the \
                 verifier rather than something resolved here",
                between.join(" and ")
            ),
        }
    }
}

impl std::error::Error for GitError {}

/// One worker's place to work.
#[derive(Clone, Debug)]
pub struct Worktree {
    /// The task this belongs to.
    pub label: String,
    /// Where the worker runs.
    pub path: PathBuf,
    /// The branch the host created for it.
    pub branch: String,
}

/// The repository a run works in, and the worktrees it has made.
///
/// Holds what belongs to this run so that discarding removes only that. A
/// cleanup that guesses is a cleanup that eventually deletes somebody's work.
#[derive(Debug)]
pub struct Workspace {
    repository: PathBuf,
    run_id: String,
    base: String,
    /// Where worktrees are put. Outside the repository, so a worker cannot
    /// reach another worktree by walking up from its own.
    root: PathBuf,
    made: Vec<Worktree>,
}

impl Workspace {
    /// Opens a repository for one run.
    ///
    /// `base` is the commit the hand-off started at, which P1.1 recorded — so
    /// every worker starts from the state the developer's request was made
    /// against, not from whatever HEAD has become since.
    pub fn open(
        repository: impl Into<PathBuf>,
        run_id: &str,
        base: &str,
        root: impl Into<PathBuf>,
    ) -> Result<Self, GitError> {
        let repository = repository.into();
        let root = root.into();
        std::fs::create_dir_all(&root)
            .map_err(|error| GitError::Unavailable(format!("{}: {error}", root.display())))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700));
        }
        let workspace = Workspace {
            repository,
            run_id: run_id.to_string(),
            base: base.to_string(),
            root,
            made: Vec::new(),
        };
        // Fail now rather than when the first worker starts.
        workspace.git(&["rev-parse", "--git-dir"], None)?;
        Ok(workspace)
    }

    /// The branch name for one task. Namespaced by run, so two runs of the same
    /// plan do not collide.
    pub fn branch_for(&self, label: &str) -> String {
        format!("timon/{}/{}", self.run_id, sanitise(label))
    }

    /// Makes a worktree for one task, branched from the run's base.
    ///
    /// The host does this, never the worker. A worker that could create its own
    /// branch could also move one.
    pub fn worktree_for(&mut self, label: &str) -> Result<Worktree, GitError> {
        let branch = self.branch_for(label);
        let path = self.root.join(sanitise(label));
        self.git(
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                &branch,
                &path.to_string_lossy(),
                &self.base,
            ],
            None,
        )?;
        let worktree = Worktree {
            label: label.to_string(),
            path,
            branch,
        };
        self.made.push(worktree.clone());
        Ok(worktree)
    }

    /// Commits whatever a worker left behind, so its changes become a ref the
    /// host can merge.
    ///
    /// Returns `None` when the worker changed nothing, which is not a failure:
    /// a task that concluded no change was needed has said something useful.
    pub fn commit_work(&self, worktree: &Worktree) -> Result<Option<String>, GitError> {
        let status = self.git(&["status", "--porcelain"], Some(&worktree.path))?;
        if status.trim().is_empty() {
            return Ok(None);
        }
        self.git(&["add", "-A"], Some(&worktree.path))?;
        self.git(
            &[
                "-c",
                "user.name=Timon",
                "-c",
                "user.email=timon@localhost",
                "commit",
                "--quiet",
                "-m",
                &format!("{}: {}", self.run_id, worktree.label),
            ],
            Some(&worktree.path),
        )?;
        Ok(Some(
            self.git(&["rev-parse", "HEAD"], Some(&worktree.path))?
                .trim()
                .to_string(),
        ))
    }

    /// Builds a commit containing the named tasks' work, for a dependant to
    /// start from.
    ///
    /// This is what makes a dependency an input rather than an ordering: the
    /// consumer starts from a tree that actually contains what it depends on.
    pub fn prepare(&mut self, merged: &[String]) -> Result<String, GitError> {
        let staging = self
            .root
            .join(format!(".prepare-{}", sanitise(&merged.join("-"))));
        let branch = format!(
            "timon/{}/prepared/{}",
            self.run_id,
            sanitise(&merged.join("-"))
        );
        self.git(
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                &branch,
                &staging.to_string_lossy(),
                &self.base,
            ],
            None,
        )?;

        for label in merged {
            let theirs = self.branch_for(label);
            // `--no-ff` so the merge is a decision that is recorded, and no
            // strategy option that would let git pick a side on a conflict.
            let result = self.git(
                &[
                    "-c",
                    "user.name=Timon",
                    "-c",
                    "user.email=timon@localhost",
                    "merge",
                    "--no-ff",
                    "--no-edit",
                    &theirs,
                ],
                Some(&staging),
            );
            if let Err(GitError::Refused { detail, .. }) = result {
                let conflicts = self
                    .git(&["diff", "--name-only", "--diff-filter=U"], Some(&staging))
                    .unwrap_or_default();
                let _ = self.git(&["merge", "--abort"], Some(&staging));
                return Err(GitError::Conflict {
                    between: merged.to_vec(),
                    detail: if conflicts.trim().is_empty() {
                        detail
                    } else {
                        format!(
                            "conflicting files: {}",
                            conflicts.split_whitespace().collect::<Vec<_>>().join(", ")
                        )
                    },
                });
            }
            result?;
        }

        let commit = self
            .git(&["rev-parse", "HEAD"], Some(&staging))?
            .trim()
            .to_string();
        // The staging worktree has done its job; the commit outlives it.
        let _ = self.git(
            &["worktree", "remove", "--force", &staging.to_string_lossy()],
            None,
        );
        Ok(commit)
    }

    /// Merges the finished work onto one branch for the developer to review.
    ///
    /// In dependency order, mechanically, one at a time so a conflict names the
    /// pair that produced it rather than the whole run.
    pub fn integrate(&mut self, order: &[String]) -> Result<String, GitError> {
        let branch = format!("timon/{}/result", self.run_id);
        let staging = self.root.join(".result");
        self.git(
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                &branch,
                &staging.to_string_lossy(),
                &self.base,
            ],
            None,
        )?;

        let mut merged_so_far: Vec<String> = Vec::new();
        for label in order {
            let theirs = self.branch_for(label);
            if self.git(&["rev-parse", "--verify", &theirs], None).is_err() {
                // A task that produced no branch produced no changes. Not an
                // error: it is a task that decided nothing needed doing.
                continue;
            }
            let result = self.git(
                &[
                    "-c",
                    "user.name=Timon",
                    "-c",
                    "user.email=timon@localhost",
                    "merge",
                    "--no-ff",
                    "--no-edit",
                    &theirs,
                ],
                Some(&staging),
            );
            if result.is_err() {
                let conflicts = self
                    .git(&["diff", "--name-only", "--diff-filter=U"], Some(&staging))
                    .unwrap_or_default();
                let _ = self.git(&["merge", "--abort"], Some(&staging));
                let mut between = merged_so_far.clone();
                between.push(label.clone());
                return Err(GitError::Conflict {
                    between,
                    detail: format!(
                        "conflicting files: {}",
                        conflicts.split_whitespace().collect::<Vec<_>>().join(", ")
                    ),
                });
            }
            merged_so_far.push(label.clone());
        }

        let _ = self.git(
            &["worktree", "remove", "--force", &staging.to_string_lossy()],
            None,
        );
        Ok(branch)
    }

    /// Removes this run's worktrees and branches, and nothing else.
    ///
    /// Explicit, and not called on failure. A failed run keeps its worktrees
    /// until somebody asks, because deleting the evidence the moment a run
    /// fails is how a failure becomes unexplainable.
    ///
    /// Returns what it removed, so a report can say rather than imply.
    pub fn discard(&mut self) -> Vec<String> {
        let mut removed = Vec::new();
        for worktree in std::mem::take(&mut self.made) {
            if self
                .git(
                    &[
                        "worktree",
                        "remove",
                        "--force",
                        &worktree.path.to_string_lossy(),
                    ],
                    None,
                )
                .is_ok()
            {
                removed.push(worktree.path.display().to_string());
            }
            if self.git(&["branch", "-D", &worktree.branch], None).is_ok() {
                removed.push(worktree.branch.clone());
            }
        }
        let _ = self.git(&["worktree", "prune"], None);
        removed
    }

    /// What this run made, for a report or a cleanup somebody else does.
    pub fn made(&self) -> &[Worktree] {
        &self.made
    }

    fn git(&self, args: &[&str], cwd: Option<&Path>) -> Result<String, GitError> {
        let mut command = Command::new("git");
        command.arg("-C").arg(cwd.unwrap_or(&self.repository));
        command.args(args);
        let output = command
            .output()
            .map_err(|error| GitError::Unavailable(error.to_string()))?;
        if output.status.success() {
            return Ok(String::from_utf8_lossy(&output.stdout).to_string());
        }
        Err(GitError::Refused {
            what: format!("git {}", args.first().copied().unwrap_or("")),
            detail: String::from_utf8_lossy(&output.stderr)
                .trim()
                .lines()
                .take(3)
                .collect::<Vec<_>>()
                .join("; "),
        })
    }
}

/// Makes a label safe for a branch name and a directory name.
///
/// Conservative: a planner chooses labels, and a label that becomes `../..` or
/// a ref-breaking sequence is not something to discover in production.
fn sanitise(label: &str) -> String {
    let cleaned: String = label
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches('-');
    if trimmed.is_empty() {
        "task".to_string()
    } else {
        trimmed.chars().take(64).collect()
    }
}
