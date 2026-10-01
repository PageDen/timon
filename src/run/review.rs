//! Taking, inspecting or throwing away what a run produced.
//!
//! A finished run leaves a branch. Until now the developer was given its name
//! and left to assemble git commands, which is a poor end to an otherwise
//! bounded piece of work: the two things anyone wants are "show me" and "keep
//! it", and both were homework.
//!
//! Nothing here invents a policy. `accept` performs the merge the developer
//! would have typed, refuses when their tree is dirty, and never pushes.
//! `discard` deletes only branches belonging to the named run.

use std::path::{Path, PathBuf};

use super::record::Run;

/// Which branch holds a run's work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Produced {
    pub branch: String,
    /// The commit the run started from, when it recorded one.
    pub base: Option<String>,
    pub workspace: PathBuf,
}

#[derive(Debug)]
pub enum ReviewError {
    /// The run never recorded which repository it was rooted in.
    NoWorkspace,
    /// No branch for this run is left. Already discarded, or it changed nothing.
    NothingProduced,
    /// More than one candidate and none of them is the integrated result.
    Ambiguous(Vec<String>),
    /// The developer has uncommitted changes, so a merge would mix them in.
    WorkspaceDirty(String),
    Git {
        what: String,
        why: String,
    },
    /// Some branches went and some did not. Both halves are named, because
    /// deleting branches is not one transaction and a developer left guessing
    /// which half survived is worse off than before they asked.
    PartlyDiscarded {
        deleted: Vec<String>,
        refused: Vec<(String, String)>,
    },
}

impl std::fmt::Display for ReviewError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReviewError::NoWorkspace => write!(
                f,
                "that run did not record a repository, so there is nothing to \
                 inspect against"
            ),
            ReviewError::NothingProduced => write!(
                f,
                "no branch is left for that run. Either it changed nothing, or \
                 it has already been discarded"
            ),
            ReviewError::Ambiguous(branches) => write!(
                f,
                "that run left several branches and no integrated result: {}. \
                 Name one with `git` directly",
                branches.join(", ")
            ),
            ReviewError::WorkspaceDirty(detail) => write!(
                f,
                "your working tree has uncommitted changes, so merging would mix \
                 them with the run's work:\n{detail}"
            ),
            ReviewError::Git { what, why } => write!(f, "{what}: {why}"),
            ReviewError::PartlyDiscarded { deleted, refused } => {
                writeln!(
                    f,
                    "only part of that run was discarded. {} branch(es) deleted:",
                    deleted.len()
                )?;
                for branch in deleted {
                    writeln!(f, "  gone  {branch}")?;
                }
                writeln!(f, "and {} that would not go:", refused.len())?;
                for (branch, why) in refused {
                    writeln!(f, "  kept  {branch}: {why}")?;
                }
                write!(f, "Run the same discard again once the reason is cleared.")
            }
        }
    }
}

impl std::error::Error for ReviewError {}

fn git(workspace: &Path, args: &[&str]) -> Result<String, ReviewError> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(args)
        .output()
        .map_err(|error| ReviewError::Git {
            what: format!("running git {}", args.join(" ")),
            why: error.to_string(),
        })?;
    if !output.status.success() {
        return Err(ReviewError::Git {
            what: format!("git {}", args.join(" ")),
            why: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .trim_end()
        .to_string())
}

/// Finds the branch holding a run's work.
///
/// The integrated result wins when there is one, because that is what the host
/// merged the workers into. A single-worker route has no result branch and its
/// one worker branch is the answer. Anything else is ambiguous and says so
/// rather than guessing which of a developer's branches to merge.
pub fn produced(run: &Run) -> Result<Produced, ReviewError> {
    let workspace = run.workspace.clone().ok_or(ReviewError::NoWorkspace)?;
    let prefix = format!("timon/{}/", run.id);
    let listed = git(
        &workspace,
        &[
            "branch",
            "--list",
            &format!("{prefix}*"),
            "--format=%(refname:short)",
        ],
    )?;
    let mut branches: Vec<String> = listed
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        // `prepared/` branches are the trees handed to dependent tasks, not
        // anybody's work to merge.
        .filter(|line| !line.starts_with(&format!("{prefix}prepared/")))
        .map(str::to_string)
        .collect();

    let result = format!("{prefix}result");
    let branch = if branches.iter().any(|b| b == &result) {
        result
    } else if branches.len() == 1 {
        branches.remove(0)
    } else if branches.is_empty() {
        return Err(ReviewError::NothingProduced);
    } else {
        branches.sort();
        return Err(ReviewError::Ambiguous(branches));
    };

    Ok(Produced {
        branch,
        base: run.base.commit().map(str::to_string),
        workspace,
    })
}

/// What a run changed, as a diff against where it started.
///
/// Against the base commit rather than the developer's current HEAD: the run
/// started from that commit, and diffing against a HEAD that has since moved
/// would attribute everyone else's work to the run.
pub fn diff(produced: &Produced, full: bool) -> Result<String, ReviewError> {
    let from = match &produced.base {
        Some(base) => base.clone(),
        None => git(
            &produced.workspace,
            &["merge-base", "HEAD", &produced.branch],
        )?,
    };
    let range = format!("{from}..{}", produced.branch);
    let mut args = vec!["diff", &range];
    if !full {
        args.push("--stat");
    }
    git(&produced.workspace, &args)
}

/// Whether the developer's tree is clean enough to merge into.
pub fn workspace_clean(workspace: &Path) -> Result<(), ReviewError> {
    let status = git(workspace, &["status", "--porcelain"])?;
    if status.is_empty() {
        return Ok(());
    }
    Err(ReviewError::WorkspaceDirty(status))
}

/// Merges a run's work into the current branch.
///
/// `--no-ff`, so the merge is a commit that names the run and can be reverted as
/// one thing. Nothing is pushed: what to do with the developer's remote is
/// theirs to decide.
pub fn accept(produced: &Produced) -> Result<String, ReviewError> {
    workspace_clean(&produced.workspace)?;
    let message = format!("Merge Timon run {}", produced.branch);
    git(
        &produced.workspace,
        &["merge", "--no-ff", "-m", &message, &produced.branch],
    )
}

/// Deletes every branch belonging to one run.
///
/// Only that run's branches, matched on its own id, and `-D` because a branch
/// nobody merged is exactly what discarding means.
pub fn discard(run: &Run) -> Result<Vec<String>, ReviewError> {
    let workspace = run.workspace.clone().ok_or(ReviewError::NoWorkspace)?;
    let prefix = format!("timon/{}/", run.id);
    let listed = git(
        &workspace,
        &[
            "branch",
            "--list",
            &format!("{prefix}*"),
            "--format=%(refname:short)",
        ],
    )?;
    let branches: Vec<String> = listed
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect();
    if branches.is_empty() {
        return Err(ReviewError::NothingProduced);
    }

    // The worker worktrees still have these branches checked out, and git will
    // not delete a branch a worktree holds. Found by discarding a real run:
    //
    //   cannot delete branch 'timon/.../write-contributing' used by worktree at
    //   '~/.local/share/timon/runs/.../worktrees/write-contributing'
    //
    // Only this run's worktrees are removed, matched on the branch they hold
    // rather than on a path pattern, so a developer's own worktree is never
    // touched however it happens to be named.
    for path in worktrees_holding(&workspace, &prefix)? {
        git(&workspace, &["worktree", "remove", "--force", &path])?;
    }

    // Every branch is attempted, and what failed is reported with what went.
    // Deleting branches is not one transaction, so an early return left some
    // already gone and said only what stopped it — a developer then had no
    // idea which half of their run still existed.
    let mut deleted = Vec::new();
    let mut refused = Vec::new();
    for branch in &branches {
        match git(&workspace, &["branch", "-D", branch]) {
            Ok(_) => deleted.push(branch.clone()),
            Err(why) => refused.push((branch.clone(), why.to_string())),
        }
    }
    if !refused.is_empty() {
        return Err(ReviewError::PartlyDiscarded { deleted, refused });
    }
    Ok(deleted)
}

/// Paths of worktrees whose checked-out branch belongs to one run.
fn worktrees_holding(workspace: &Path, prefix: &str) -> Result<Vec<String>, ReviewError> {
    let listed = git(workspace, &["worktree", "list", "--porcelain"])?;
    let mut holding = Vec::new();
    let mut path: Option<String> = None;
    for line in listed.lines() {
        if let Some(rest) = line.strip_prefix("worktree ") {
            path = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("branch ") {
            let branch = rest.trim().trim_start_matches("refs/heads/");
            if branch.starts_with(prefix)
                && let Some(found) = path.take()
            {
                holding.push(found);
            }
        }
    }
    Ok(holding)
}
