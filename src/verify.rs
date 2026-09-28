// Adapted for Timon. Not derived from Prodex source.
//! Deciding what a result is worth, and refusing to say more than was checked.
//!
//! Until now every planner run ended with a line admitting nobody had judged
//! the result. This is the judgement — and most of the care here is in what it
//! declines to claim.
//!
//! **Execution success is not task success.** A change can compile and pass
//! every test without doing what was asked. A quotation can appear on a page
//! without supporting the claim it was cited for. Collapsing those into one
//! verdict is how a report ends up saying "correct" about something nobody
//! checked, so the report carries five dimensions and keeps them apart.
//!
//! **`NotEstablished` is a value, not a gap.** It is what most results honestly
//! report at first, and a system that had no way to say it would be forced to
//! choose between two lies. Codex's review asked for exactly this.
//!
//! What this does *not* do is decide whether an answer is good. Nothing here
//! reads prose and forms an opinion. Every check is a mechanical fact — a
//! process exited, a file merged, a named test passed, a quotation was found on
//! the page it was attributed to — and the one thing that would need judgement,
//! whether a claim follows from its evidence, is reported as unchecked.

use serde::Serialize;

/// Whether a run did what it was asked, separated from whether it ran.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Standing {
    /// Checked, and it holds.
    Holds,
    /// Checked, and it does not.
    Fails,
    /// Not checked, or not checkable. **Not** a pass.
    NotEstablished,
    /// Nothing to check in this dimension for this kind of work.
    NotApplicable,
}

impl Standing {
    /// True only when something was actually checked and held.
    ///
    /// Written as a method so that no caller can get away with treating
    /// `NotEstablished` as a pass by pattern-matching on the negative.
    pub fn affirmative(self) -> bool {
        self == Standing::Holds
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Standing::Holds => "holds",
            Standing::Fails => "fails",
            Standing::NotEstablished => "not established",
            Standing::NotApplicable => "not applicable",
        }
    }
}

/// The five things a report keeps apart.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Dimension {
    /// Did it run to completion, or time out, or get cancelled.
    Execution,
    /// Schema valid, branches integrated.
    MechanicalValidation,
    /// Build and named tests.
    AutomatedChecks,
    /// The criteria written before execution.
    TaskAcceptance,
    /// Sources checked; semantic support not checked.
    Evidence,
}

impl Dimension {
    pub fn all() -> [Dimension; 5] {
        [
            Dimension::Execution,
            Dimension::MechanicalValidation,
            Dimension::AutomatedChecks,
            Dimension::TaskAcceptance,
            Dimension::Evidence,
        ]
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Dimension::Execution => "execution",
            Dimension::MechanicalValidation => "mechanical validation",
            Dimension::AutomatedChecks => "automated checks",
            Dimension::TaskAcceptance => "task acceptance",
            Dimension::Evidence => "evidence",
        }
    }
}

/// One dimension's verdict, and what it rests on.
#[derive(Clone, Debug, Serialize)]
pub struct Judgement {
    pub dimension: Dimension,
    pub standing: Standing,
    /// What was actually checked, in the terms it was checked in. A verdict
    /// without this is an opinion.
    pub detail: String,
}

/// What the developer is told about a result.
#[derive(Clone, Debug, Serialize)]
pub struct Verdict {
    pub outcome: Outcome,
    pub judgements: Vec<Judgement>,
    /// The commit that was tested, so a disagreement is settled against a tree
    /// rather than against a memory.
    pub tested: Option<String>,
    /// Commands that were run, verbatim, so somebody can repeat them.
    pub commands: Vec<String>,
}

/// The three-way outcome, derived from the judgements rather than asserted.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Outcome {
    /// Every applicable check holds, **and** task acceptance was established.
    Pass,
    /// A specific, named defect a bounded retry could address.
    Repairable { defect: String },
    /// Cannot be verified, or cannot be fixed within limits.
    Blocked { why: String },
}

impl Verdict {
    /// The single line a person reads first, written so it cannot be misread as
    /// more than it is.
    pub fn headline(&self) -> String {
        match &self.outcome {
            Outcome::Pass => {
                let checked = self
                    .judgements
                    .iter()
                    .filter(|j| j.standing.affirmative())
                    .count();
                format!("passes the {checked} check(s) that were run")
            }
            Outcome::Repairable { defect } => format!("repairable: {defect}"),
            Outcome::Blocked { why } => format!("blocked: {why}"),
        }
    }

    /// True when the thing the developer asked for was actually established.
    ///
    /// Separate from `Outcome::Pass` on purpose: a result can pass every check
    /// that was run while nobody ever checked whether it did the job.
    pub fn task_established(&self) -> bool {
        self.judgements
            .iter()
            .any(|j| j.dimension == Dimension::TaskAcceptance && j.standing.affirmative())
    }

    pub fn render(&self) -> String {
        let mut out = format!("{}\n\n", self.headline());
        for judgement in &self.judgements {
            out.push_str(&format!(
                "  {:<22} {:<16} {}\n",
                judgement.dimension.as_str(),
                judgement.standing.as_str(),
                judgement.detail
            ));
        }
        if let Some(tested) = &self.tested {
            out.push_str(&format!("\n  tested at {tested}\n"));
        }
        if !self.commands.is_empty() {
            out.push_str("  commands run:\n");
            for command in &self.commands {
                out.push_str(&format!("    {command}\n"));
            }
        }
        if !self.task_established() {
            out.push_str(
                "\n  Task acceptance was not established. Whatever else passed, \n\
                 nobody checked that this did what was asked.\n",
            );
        }
        out
    }
}

/// What the verifier was given to check.
pub struct Subject<'a> {
    /// How the run itself ended.
    pub execution: &'a crate::dag_run::GraphReport,
    /// The branch the host merged, when integration got that far.
    pub branch: Option<&'a str>,
    /// The commit that branch points at.
    pub tested: Option<String>,
    /// Acceptance criteria written before execution, as the planner stated
    /// them. Empty means nobody wrote any — which is reported, not excused.
    pub criteria: Vec<String>,
}

/// How to run the project's own checks.
///
/// A trait because the commands belong to the repository, not to Timon, and
/// because a verifier that cannot be tested without a build is a verifier
/// nobody will test.
pub trait Checks {
    /// Runs the project's build and tests. Returns what ran and whether it
    /// passed, or `None` when the project declares none.
    fn automated(&self) -> Option<CheckRun>;

    /// Whether the named acceptance criteria were met.
    ///
    /// Returning `NotEstablished` is the honest default and the expected one:
    /// deciding whether a criterion in prose was met is judgement, and this
    /// module does not do judgement.
    fn acceptance(&self, _criteria: &[String]) -> (Standing, String) {
        (
            Standing::NotEstablished,
            "no mechanical check exists for these criteria, so nobody has \
             confirmed the work does what was asked"
                .to_string(),
        )
    }
}

/// What running a project's checks produced.
#[derive(Clone, Debug, Serialize)]
pub struct CheckRun {
    pub passed: bool,
    /// How many tests ran, when the project says. A repository with few tests
    /// gets a weak check, and the number is how a reader knows that.
    pub tests_run: Option<u32>,
    pub commands: Vec<String>,
    pub detail: String,
}

/// Judges a result.
pub fn verify<C: Checks>(subject: &Subject<'_>, checks: &C) -> Verdict {
    let mut judgements = Vec::new();
    let mut commands = Vec::new();

    // Execution: did the work run at all.
    let failed: Vec<&str> = subject
        .execution
        .tasks
        .iter()
        .filter(|task| matches!(task.outcome, crate::dag_run::Outcome::Failed { .. }))
        .map(|task| task.label.as_str())
        .collect();
    let execution = if subject.execution.complete {
        Judgement {
            dimension: Dimension::Execution,
            standing: Standing::Holds,
            detail: format!("all {} task(s) completed", subject.execution.tasks.len()),
        }
    } else if failed.is_empty() {
        Judgement {
            dimension: Dimension::Execution,
            standing: Standing::Fails,
            detail: format!("{} task(s) never ran", subject.execution.not_run.len()),
        }
    } else {
        Judgement {
            dimension: Dimension::Execution,
            standing: Standing::Fails,
            detail: format!("failed: {}", failed.join(", ")),
        }
    };
    judgements.push(execution);

    // Mechanical validation: did the branches merge into something.
    judgements.push(match subject.branch {
        Some(branch) => Judgement {
            dimension: Dimension::MechanicalValidation,
            standing: Standing::Holds,
            detail: format!("worker branches merged into {branch}"),
        },
        None => Judgement {
            dimension: Dimension::MechanicalValidation,
            standing: Standing::NotEstablished,
            detail: "no branch was produced, so there was nothing to merge".to_string(),
        },
    });

    // Automated checks: the project's own build and tests.
    judgements.push(match checks.automated() {
        Some(run) => {
            commands.extend(run.commands.clone());
            let counted = match run.tests_run {
                Some(n) => format!("{n} test(s) ran"),
                None => "the project did not say how many tests ran".to_string(),
            };
            Judgement {
                dimension: Dimension::AutomatedChecks,
                standing: if run.passed {
                    Standing::Holds
                } else {
                    Standing::Fails
                },
                detail: format!("{}; {counted}", run.detail),
            }
        }
        None => Judgement {
            dimension: Dimension::AutomatedChecks,
            standing: Standing::NotApplicable,
            detail: "this project declares no build or tests to run".to_string(),
        },
    });

    // Task acceptance: the part that is usually not established, and the part
    // that matters most.
    judgements.push(if subject.criteria.is_empty() {
        Judgement {
            dimension: Dimension::TaskAcceptance,
            standing: Standing::NotEstablished,
            detail: "no acceptance criteria were written before this ran, so there \
                     is nothing to check the work against"
                .to_string(),
        }
    } else {
        let (standing, detail) = checks.acceptance(&subject.criteria);
        Judgement {
            dimension: Dimension::TaskAcceptance,
            standing,
            detail,
        }
    });

    // Evidence: what was checked about sources, and what was not.
    judgements.push(Judgement {
        dimension: Dimension::Evidence,
        standing: Standing::NotEstablished,
        detail: "semantic support is unimplemented: whether a claim follows from \
                 the source it cites has not been checked"
            .to_string(),
    });

    let outcome = decide(&judgements);
    Verdict {
        outcome,
        judgements,
        tested: subject.tested.clone(),
        commands,
    }
}

/// Derives the outcome from the judgements rather than asserting one.
///
/// The ordering is deliberate. A failure that a retry could address is
/// repairable; one that it could not is blocked; and a pass requires that task
/// acceptance was actually established, not merely that nothing failed.
fn decide(judgements: &[Judgement]) -> Outcome {
    let find = |dimension: Dimension| {
        judgements
            .iter()
            .find(|j| j.dimension == dimension)
            .expect("every dimension is judged")
    };

    let execution = find(Dimension::Execution);
    if execution.standing == Standing::Fails {
        // A task that failed can be retried; the defect is named so a repair
        // has something to aim at.
        return Outcome::Repairable {
            defect: execution.detail.clone(),
        };
    }

    let automated = find(Dimension::AutomatedChecks);
    if automated.standing == Standing::Fails {
        return Outcome::Repairable {
            defect: format!(
                "the project's own checks did not pass: {}",
                automated.detail
            ),
        };
    }

    let mechanical = find(Dimension::MechanicalValidation);
    if mechanical.standing == Standing::NotEstablished {
        return Outcome::Blocked {
            why: mechanical.detail.clone(),
        };
    }

    let acceptance = find(Dimension::TaskAcceptance);
    if acceptance.standing == Standing::Fails {
        return Outcome::Repairable {
            defect: acceptance.detail.clone(),
        };
    }

    Outcome::Pass
}

/// Runs a repository's own build and tests.
///
/// What counts as "the project's checks" belongs to the repository, not to
/// Timon. This looks for a declaration and runs nothing it was not told about:
/// guessing a build command means running arbitrary commands in somebody's
/// checkout on the strength of a filename.
pub struct ProjectChecks {
    repository: Option<std::path::PathBuf>,
    branch: Option<String>,
    /// The command a project declares, as `timon-checks` in its git config.
    declared: Option<String>,
}

impl ProjectChecks {
    /// Reads what a repository says its checks are.
    ///
    /// `git config timon.checks` — set by whoever owns the repository, which is
    /// the only person who should be choosing what Timon executes in it.
    pub fn for_repository(repository: Option<&std::path::Path>, branch: Option<&str>) -> Self {
        let declared = repository.and_then(|path| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(path)
                .args(["config", "--get", "timon.checks"])
                .output()
                .ok()?;
            let value = String::from_utf8_lossy(&out.stdout).trim().to_string();
            (out.status.success() && !value.is_empty()).then_some(value)
        });
        ProjectChecks {
            repository: repository.map(std::path::Path::to_path_buf),
            branch: branch.map(str::to_string),
            declared,
        }
    }
}

impl Checks for ProjectChecks {
    fn automated(&self) -> Option<CheckRun> {
        let repository = self.repository.as_ref()?;
        let declared = self.declared.as_ref()?;
        let branch = self.branch.as_ref()?;

        // Checked on a worktree of the branch, never on the developer's own
        // files: running a project's test suite is running its code, and that
        // is not something to do in somebody's working directory.
        let checkout = std::env::temp_dir().join(format!("timon-checks-{}", std::process::id()));
        let add = std::process::Command::new("git")
            .arg("-C")
            .arg(repository)
            .args([
                "worktree",
                "add",
                "--quiet",
                "--detach",
                &checkout.to_string_lossy(),
                branch,
            ])
            .output()
            .ok()?;
        if !add.status.success() {
            return Some(CheckRun {
                passed: false,
                tests_run: None,
                commands: vec![format!("git worktree add … {branch}")],
                detail: "the branch could not be checked out to run its tests".to_string(),
            });
        }

        let run = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(declared)
            .current_dir(&checkout)
            .output();

        let _ = std::process::Command::new("git")
            .arg("-C")
            .arg(repository)
            .args(["worktree", "remove", "--force", &checkout.to_string_lossy()])
            .output();

        let output = run.ok()?;
        let text = String::from_utf8_lossy(&output.stdout).to_string()
            + &String::from_utf8_lossy(&output.stderr);
        Some(CheckRun {
            passed: output.status.success(),
            tests_run: count_tests(&text),
            commands: vec![declared.clone()],
            detail: if output.status.success() {
                "the project's declared checks passed".to_string()
            } else {
                format!(
                    "the project's declared checks failed: {}",
                    text.lines().rev().take(2).collect::<Vec<_>>().join(" ")
                )
            },
        })
    }
}

/// Counts tests from output that says so, and declines to guess when it does not.
fn count_tests(text: &str) -> Option<u32> {
    let mut total = 0u32;
    let mut found = false;
    for line in text.lines() {
        if let Some(rest) = line.trim().strip_prefix("test result:")
            && let Some(passed) = rest.split_whitespace().nth(1)
            && let Ok(count) = passed.parse::<u32>()
        {
            total += count;
            found = true;
        }
    }
    found.then_some(total)
}
