// Adapted for Timon. Not derived from Prodex source.
//! Criteria a machine can actually check.
//!
//! The verifier reported task acceptance as *not established* on every run, for
//! a good reason: deciding whether a sentence of prose was satisfied is
//! judgement, and judgement is what this system does not do about its own work.
//!
//! So a criterion is not a sentence. It is a claim about the resulting tree that
//! is true or false by inspection — a file exists, a file contains this text, a
//! file no longer mentions that. The planner writes them **before** the work
//! runs, which is the only time they can be written honestly: criteria invented
//! afterwards describe what happened rather than what was wanted.
//!
//! **A criterion is data, not authority.** It cannot run a command, reach the
//! network, or name a path outside the tree it is checked against. A model
//! choosing what to execute would be the model choosing its own permissions,
//! and the rest of this design spends considerable effort preventing exactly
//! that. Running the project's own checks is a separate thing, declared by
//! whoever owns the repository.

use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

/// One checkable claim about the result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Criterion {
    /// A file the work was supposed to produce.
    FileExists { path: String },
    /// A file the work was supposed to remove.
    FileAbsent { path: String },
    /// Text a file must contain. Matched literally, because a planner writing a
    /// regex is a planner writing something nobody will read carefully.
    FileContains { path: String, text: String },
    /// Text a file must no longer contain.
    FileOmits { path: String, text: String },
}

impl Criterion {
    /// What this says, in a line a person can check by hand.
    pub fn describe(&self) -> String {
        match self {
            Criterion::FileExists { path } => format!("{path} exists"),
            Criterion::FileAbsent { path } => format!("{path} is gone"),
            Criterion::FileContains { path, text } => {
                format!("{path} contains {:?}", elide(text))
            }
            Criterion::FileOmits { path, text } => {
                format!("{path} no longer contains {:?}", elide(text))
            }
        }
    }

    fn path(&self) -> &str {
        match self {
            Criterion::FileExists { path }
            | Criterion::FileAbsent { path }
            | Criterion::FileContains { path, .. }
            | Criterion::FileOmits { path, .. } => path,
        }
    }
}

fn elide(text: &str) -> String {
    if text.chars().count() <= 40 {
        text.to_string()
    } else {
        format!("{}…", text.chars().take(40).collect::<String>())
    }
}

/// What checking one criterion found.
#[derive(Clone, Debug, Serialize)]
pub struct Checked {
    pub criterion: Criterion,
    pub met: bool,
    /// Why, in the terms it was checked in.
    pub detail: String,
}

/// Why a criterion could not be checked at all.
///
/// Separate from failing it: a criterion nobody could evaluate has established
/// nothing, and calling that a failure would send somebody fixing the wrong
/// thing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub enum Unusable {
    /// A path that leaves the tree, or is absolute.
    PathEscapes { path: String },
    /// An empty path or empty text.
    Empty,
}

impl std::fmt::Display for Unusable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Unusable::PathEscapes { path } => write!(
                f,
                "{path:?} is outside the tree being checked, so it is not a claim \
                 about this work"
            ),
            Unusable::Empty => {
                write!(f, "a criterion with no path or no text checks nothing")
            }
        }
    }
}

/// Resolves a criterion's path inside the tree, or refuses it.
///
/// Refuses absolute paths and anything with a parent component, then checks
/// where it really points — a symlink inside the tree aimed out of it is the
/// other way this goes wrong.
pub fn resolve(tree: &Path, path: &str) -> Result<PathBuf, Unusable> {
    if path.trim().is_empty() {
        return Err(Unusable::Empty);
    }
    let candidate = Path::new(path);
    if candidate.is_absolute()
        || candidate
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::RootDir))
    {
        return Err(Unusable::PathEscapes {
            path: path.to_string(),
        });
    }
    let joined = tree.join(candidate);
    if let Ok(real) = joined.canonicalize() {
        let root = tree.canonicalize().unwrap_or_else(|_| tree.to_path_buf());
        if !real.starts_with(&root) {
            return Err(Unusable::PathEscapes {
                path: path.to_string(),
            });
        }
        return Ok(real);
    }
    Ok(joined)
}

/// Checks one criterion against a tree.
pub fn check(tree: &Path, criterion: &Criterion) -> Result<Checked, Unusable> {
    let path = resolve(tree, criterion.path())?;
    let exists = path.exists();

    let (met, detail) = match criterion {
        Criterion::FileExists { .. } => (
            exists,
            if exists { "present" } else { "not present" }.to_string(),
        ),
        Criterion::FileAbsent { .. } => (
            !exists,
            if exists { "still present" } else { "absent" }.to_string(),
        ),
        Criterion::FileContains { text, .. } => {
            if text.is_empty() {
                return Err(Unusable::Empty);
            }
            match std::fs::read_to_string(&path) {
                Ok(content) => {
                    let found = content.contains(text.as_str());
                    (
                        found,
                        if found {
                            "found"
                        } else {
                            "not found in the file"
                        }
                        .to_string(),
                    )
                }
                Err(_) if !exists => (false, "the file does not exist".to_string()),
                Err(error) => (false, format!("could not be read: {error}")),
            }
        }
        Criterion::FileOmits { text, .. } => {
            if text.is_empty() {
                return Err(Unusable::Empty);
            }
            match std::fs::read_to_string(&path) {
                // A file that is gone omits everything, which is what was asked.
                Err(_) if !exists => (true, "the file does not exist".to_string()),
                Ok(content) => {
                    let found = content.contains(text.as_str());
                    (
                        !found,
                        if found { "still there" } else { "gone" }.to_string(),
                    )
                }
                Err(error) => (false, format!("could not be read: {error}")),
            }
        }
    };

    Ok(Checked {
        criterion: criterion.clone(),
        met,
        detail,
    })
}

/// What checking a whole set found.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Report {
    pub checked: Vec<Checked>,
    /// Criteria that could not be evaluated, with why. Not failures.
    pub unusable: Vec<(Criterion, String)>,
}

impl Report {
    /// True only when something was checked and all of it held.
    ///
    /// An empty set establishes nothing, and neither does a set where every
    /// criterion turned out to be unusable.
    pub fn established(&self) -> bool {
        !self.checked.is_empty() && self.checked.iter().all(|c| c.met)
    }

    /// One line, naming what failed rather than counting it.
    pub fn summary(&self) -> String {
        let failed: Vec<String> = self
            .checked
            .iter()
            .filter(|c| !c.met)
            .map(|c| format!("{} ({})", c.criterion.describe(), c.detail))
            .collect();
        let mut out = if self.checked.is_empty() {
            "no criterion could be checked".to_string()
        } else if failed.is_empty() {
            format!("all {} criterion/criteria hold", self.checked.len())
        } else {
            format!(
                "{} of {} failed: {}",
                failed.len(),
                self.checked.len(),
                failed.join("; ")
            )
        };
        if !self.unusable.is_empty() {
            out.push_str(&format!(
                ". {} could not be checked: {}",
                self.unusable.len(),
                self.unusable
                    .iter()
                    .map(|(criterion, why)| format!("{} — {why}", criterion.describe()))
                    .collect::<Vec<_>>()
                    .join("; ")
            ));
        }
        out
    }
}

/// Checks a set of criteria against a tree.
pub fn check_all(tree: &Path, criteria: &[Criterion]) -> Report {
    let mut report = Report::default();
    for criterion in criteria {
        match check(tree, criterion) {
            Ok(checked) => report.checked.push(checked),
            Err(why) => report.unusable.push((criterion.clone(), why.to_string())),
        }
    }
    report
}
