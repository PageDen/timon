//! Measuring the write sandbox on this host, so writing can be enabled honestly.
//!
//! The first qualification was taken by hand, through `codex exec`, on one
//! Linux host, and the record was then copied into place. That works for one
//! machine and nowhere else: a Mac sandboxes differently (Seatbelt rather than
//! bubblewrap and Landlock), so a record from Linux says nothing about it.
//!
//! This runs the nine checks for real. Each one is attempted through `codex
//! sandbox` with exactly the settings a writing worker gets — `workspace-write`,
//! network off — from inside a worktree of a throwaway repository, the same
//! shape a real run has. No model is called, so it costs nothing and gives the
//! same answer every time.
//!
//! Every result is read back from the filesystem, the repository or the process
//! table afterwards. What a probe printed about itself is not evidence.
//!
//! The scratch repository lives under the developer's Timon directory and never
//! under `/tmp`: the write sandbox allows `/tmp` and `$TMPDIR`, so a target
//! there would be writable for a reason that has nothing to do with the
//! worktree, and "write outside the worktree" would fail for the wrong reason.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Serialize;

use crate::qualify::{Check, Expected, Finding, Observed, Qualification};

/// The sandbox a writing worker gets, which is the only one worth measuring.
/// Kept beside `run::execute::worker_command` so the two cannot drift apart
/// without a test noticing.
pub const SANDBOX_SETTINGS: [&str; 2] = [
    "sandbox_mode=\"workspace-write\"",
    "sandbox_workspace_write.network_access=false",
];

/// Where the pooled credentials live when the broker runs on this host.
pub const BROKER_STORE: &str = "/var/lib/timon-broker/accounts";

/// One run of the probe, in the shape `timon qualify` reads back.
#[derive(Debug, Serialize)]
pub struct Run {
    pub taken_at: i64,
    pub verdict: &'static str,
    pub findings: Vec<Finding>,
    pub what_this_does_not_cover: Vec<&'static str>,
}

/// Runs every check. `codex` is the program to sandbox commands with.
pub async fn probe(
    codex: &str,
    scratch: &Path,
    host: &str,
    now: i64,
) -> Result<Qualification, String> {
    refuse_temporary(scratch)?;
    let lab = Lab::build(scratch)?;

    let mut findings = vec![
        write_inside(codex, &lab),
        write_outside(codex, &lab),
        shared_git_metadata(codex, &lab),
        move_refs(codex, &lab),
        repository_hooks(codex, &lab),
        reach_remote(codex, &lab),
        read_credentials(codex, &lab),
        read_another_account(codex, &lab),
    ];
    findings.push(processes_left_behind(codex, &lab).await);

    Ok(Qualification {
        findings,
        host: host.to_string(),
        taken_at: now,
    })
}

/// What a passing run still does not establish. Written into every record.
pub const DOES_NOT_COVER: [&str; 3] = [
    "A developer's own Codex login under ~/.codex is readable by their own workers. \
     That is inherent to running as them; read_credentials is about the pooled store.",
    "leave_processes_behind covers a plain background process. A worker that starts \
     a new session to leave its process group is not covered.",
    "A different Codex version re-opens every result here.",
];

fn refuse_temporary(scratch: &Path) -> Result<(), String> {
    let temporary = [
        PathBuf::from("/tmp"),
        PathBuf::from("/private/tmp"),
        std::env::temp_dir(),
    ];
    if temporary.iter().any(|t| scratch.starts_with(t)) {
        return Err(format!(
            "{} is under a temporary directory, which the write sandbox allows. A probe \
             there would measure that allowance, not the worktree boundary",
            scratch.display()
        ));
    }
    Ok(())
}

/// The throwaway repository and worktree the checks run against.
struct Lab {
    root: PathBuf,
    repo: PathBuf,
    worktree: PathBuf,
}

impl Lab {
    fn build(root: &Path) -> Result<Lab, String> {
        let make = |path: &Path| {
            std::fs::create_dir_all(path).map_err(|e| format!("creating {}: {e}", path.display()))
        };
        make(root)?;
        let repo = root.join("repo");
        let worktree = root.join("worktree");
        make(&repo)?;
        git(&repo, &["init", "-q"])?;
        git(&repo, &["config", "user.email", "probe@timon"])?;
        git(&repo, &["config", "user.name", "Timon probe"])?;
        std::fs::write(repo.join("README.md"), "probe\n").map_err(|e| e.to_string())?;
        git(&repo, &["add", "-A"])?;
        git(&repo, &["commit", "-qm", "initial"])?;
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                &worktree.to_string_lossy(),
                "-b",
                "timon/probe/worktree",
            ],
        )?;
        // A hook that leaves a mark *inside* the worktree, where the worker can
        // write. If it ever ran, the mark would appear; a mark placed outside
        // would be stopped by the sandbox and prove nothing about the hook.
        let hook = repo.join(".git").join("hooks").join("pre-commit");
        std::fs::write(
            &hook,
            format!(
                "#!/bin/sh\necho ran > \"{}/hook-ran\"\n",
                worktree.display()
            ),
        )
        .map_err(|e| e.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755))
                .map_err(|e| e.to_string())?;
        }
        Ok(Lab {
            root: root.to_path_buf(),
            repo,
            worktree,
        })
    }
}

fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|e| format!("git {}: {e}", args.join(" ")))?;
    if !output.status.success() {
        return Err(format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Printed first by every check, so a check whose command never ran can be
/// told apart from one the sandbox stopped.
pub const RAN: &str = "TIMON-PROBE-RAN";

/// The `codex sandbox` invocation a check runs through, from the working
/// directory the caller sets.
///
/// Not `-C`: on Codex 0.155.1 it demands `--permission-profile`, and the whole
/// invocation is rejected. The first run of this probe passed `-C`, so no check
/// ran at all — and six of them reported `blocked`, because a command that
/// never executes writes nothing. That is why `RAN` exists.
pub fn sandboxed(codex: &str, script: &str) -> Vec<String> {
    let mut args = vec![codex.to_string(), "sandbox".into()];
    for setting in SANDBOX_SETTINGS {
        args.push("-c".into());
        args.push(setting.into());
    }
    args.extend([
        "--".into(),
        "sh".into(),
        "-c".into(),
        format!("echo {RAN}; {script}"),
    ]);
    args
}

/// Runs a script in the sandbox. The output is kept only for the evidence line.
fn attempt(codex: &str, lab: &Lab, script: &str) -> Result<String, String> {
    let args = sandboxed(codex, script);
    let output = Command::new(&args[0])
        .args(&args[1..])
        .current_dir(&lab.worktree)
        .output()
        .map_err(|e| format!("could not run {codex} sandbox: {e}"))?;
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    if !text.contains(RAN) {
        return Err(format!(
            "the sandboxed command never ran, so nothing was learned: {}",
            first_line(text.trim())
        ));
    }
    Ok(text.replace(RAN, "").trim().to_string())
}

fn finding(
    check: Check,
    expected: Expected,
    observed: Observed,
    probe: &str,
    evidence: String,
) -> Finding {
    Finding {
        check,
        expected,
        observed,
        probe: probe.to_string(),
        evidence,
    }
}

fn not_run(check: Check, expected: Expected, probe: &str, why: String) -> Finding {
    finding(check, expected, Observed::NotRun, probe, why)
}

fn first_line(text: &str) -> String {
    text.lines()
        .next()
        .unwrap_or("")
        .chars()
        .take(160)
        .collect()
}

fn write_inside(codex: &str, lab: &Lab) -> Finding {
    let check = Check::WriteInsideWorktree;
    let script = "echo timon-probe > probe-inside.txt";
    let output = match attempt(codex, lab, script) {
        Ok(out) => out,
        Err(why) => return not_run(check, Expected::Allowed, script, why),
    };
    let written = std::fs::read_to_string(lab.worktree.join("probe-inside.txt"))
        .map(|s| s.trim() == "timon-probe")
        .unwrap_or(false);
    let observed = if written {
        Observed::Allowed
    } else {
        Observed::Blocked
    };
    finding(
        check,
        Expected::Allowed,
        observed,
        script,
        if written {
            "probe-inside.txt exists in the worktree with the expected content".into()
        } else {
            format!("probe-inside.txt is missing: {}", first_line(&output))
        },
    )
}

fn write_outside(codex: &str, lab: &Lab) -> Finding {
    let check = Check::WriteOutsideWorktree;
    let outside = lab.root.join("outside.txt");
    let readme = lab.repo.join("README.md");
    let script = format!(
        "echo timon-probe > \"{}\"; echo timon-probe >> \"{}\"",
        outside.display(),
        readme.display()
    );
    let output = match attempt(codex, lab, &script) {
        Ok(out) => out,
        Err(why) => return not_run(check, Expected::Blocked, &script, why),
    };
    let wrote_outside = outside.exists();
    let changed_tree = std::fs::read_to_string(&readme)
        .map(|s| s.contains("timon-probe"))
        .unwrap_or(false);
    let observed = if wrote_outside || changed_tree {
        Observed::Allowed
    } else {
        Observed::Blocked
    };
    finding(
        check,
        Expected::Blocked,
        observed,
        &script,
        match (wrote_outside, changed_tree) {
            (false, false) => format!(
                "neither a file beside the repository nor the developer's README changed ({})",
                first_line(&output)
            ),
            (true, _) => format!("{} was created", outside.display()),
            (false, true) => "the developer's README.md was modified".to_string(),
        },
    )
}

fn shared_git_metadata(codex: &str, lab: &Lab) -> Finding {
    let check = Check::ModifySharedGitMetadata;
    let hooks = lab.repo.join(".git").join("hooks").join("probe-planted");
    let script = format!(
        "git config timon.probe touched; echo planted > \"{}\"",
        hooks.display()
    );
    let output = match attempt(codex, lab, &script) {
        Ok(out) => out,
        Err(why) => return not_run(check, Expected::Blocked, &script, why),
    };
    let config_changed = git(&lab.repo, &["config", "--get", "timon.probe"])
        .map(|v| v == "touched")
        .unwrap_or(false);
    let hook_planted = hooks.exists();
    let observed = if config_changed || hook_planted {
        Observed::Allowed
    } else {
        Observed::Blocked
    };
    finding(
        check,
        Expected::Blocked,
        observed,
        &script,
        match (config_changed, hook_planted) {
            (false, false) => format!(
                "the shared .git/config and .git/hooks are unchanged ({})",
                first_line(&output)
            ),
            (true, _) => "the shared .git/config gained timon.probe".into(),
            (false, true) => "a file was planted in the shared .git/hooks".into(),
        },
    )
}

fn move_refs(codex: &str, lab: &Lab) -> Finding {
    let check = Check::MoveRefs;
    let script = "git branch timon-probe-ref";
    let output = match attempt(codex, lab, script) {
        Ok(out) => out,
        Err(why) => return not_run(check, Expected::Blocked, script, why),
    };
    let created = git(
        &lab.repo,
        &["show-ref", "--verify", "refs/heads/timon-probe-ref"],
    )
    .is_ok();
    finding(
        check,
        Expected::Blocked,
        if created {
            Observed::Allowed
        } else {
            Observed::Blocked
        },
        script,
        if created {
            "refs/heads/timon-probe-ref exists in the repository".into()
        } else {
            format!("no such ref was created ({})", first_line(&output))
        },
    )
}

fn repository_hooks(codex: &str, lab: &Lab) -> Finding {
    let check = Check::RunRepositoryHooks;
    let script = "echo change > hooked.txt && git add hooked.txt && git commit -qm probe";
    let output = match attempt(codex, lab, script) {
        Ok(out) => out,
        Err(why) => return not_run(check, Expected::Blocked, script, why),
    };
    let ran = lab.worktree.join("hook-ran").exists();
    finding(
        check,
        Expected::Blocked,
        if ran {
            Observed::Allowed
        } else {
            Observed::Blocked
        },
        script,
        if ran {
            "the repository's pre-commit hook ran: hook-ran appeared in the worktree".into()
        } else {
            format!(
                "the pre-commit hook did not run; committing was refused before it could ({})",
                first_line(&output)
            )
        },
    )
}

fn reach_remote(codex: &str, lab: &Lab) -> Finding {
    let check = Check::ReachGitRemote;
    let remote = "https://github.com/git/git";
    let script = format!("git ls-remote {remote} HEAD");
    let output = match attempt(codex, lab, &script) {
        Ok(out) => out,
        Err(why) => return not_run(check, Expected::Blocked, &script, why),
    };
    if has_object_id(&output) {
        return finding(
            check,
            Expected::Blocked,
            Observed::Allowed,
            &script,
            format!(
                "the remote answered from inside the sandbox: {}",
                first_line(&output)
            ),
        );
    }
    // Refused inside. Only evidence if the host itself can reach it.
    let host_reaches = git(&lab.repo, &["ls-remote", remote, "HEAD"])
        .map(|out| has_object_id(&out))
        .unwrap_or(false);
    if !host_reaches {
        return not_run(
            check,
            Expected::Blocked,
            &script,
            format!(
                "this host could not reach {remote} outside the sandbox either, so nothing was learned"
            ),
        );
    }
    finding(
        check,
        Expected::Blocked,
        Observed::Blocked,
        &script,
        format!(
            "the host reaches it, the sandbox did not ({})",
            first_line(&output)
        ),
    )
}

fn has_object_id(text: &str) -> bool {
    text.split_whitespace()
        .any(|word| word.len() == 40 && word.chars().all(|c| c.is_ascii_hexdigit()))
}

fn read_credentials(codex: &str, lab: &Lab) -> Finding {
    let check = Check::ReadCredentials;
    let script = format!("ls \"{BROKER_STORE}\" >/dev/null 2>&1 && echo LISTED || echo REFUSED");
    if !Path::new(BROKER_STORE).exists() {
        // Checking existence from outside the sandbox can itself be refused by
        // permissions; a store that exists but is unreadable to this user is
        // tested below. Only a genuinely absent path short-circuits.
        if std::fs::symlink_metadata(BROKER_STORE).is_err()
            && std::fs::symlink_metadata(Path::new(BROKER_STORE).parent().unwrap_or(Path::new("/")))
                .is_err()
        {
            return finding(
                check,
                Expected::Blocked,
                Observed::Blocked,
                &script,
                "no pooled credential store on this host: the broker that holds it runs \
                 elsewhere, so no worker here can read it"
                    .into(),
            );
        }
    }
    let output = match attempt(codex, lab, &script) {
        Ok(out) => out,
        Err(why) => return not_run(check, Expected::Blocked, &script, why),
    };
    let denied = !output.contains("LISTED");
    finding(
        check,
        Expected::Blocked,
        if denied {
            Observed::Blocked
        } else {
            Observed::Allowed
        },
        &script,
        if denied {
            format!(
                "the pooled store could not be listed ({})",
                first_line(&output)
            )
        } else {
            format!(
                "the pooled store was listed from inside the sandbox: {}",
                first_line(&output)
            )
        },
    )
}

fn read_another_account(codex: &str, lab: &Lab) -> Finding {
    let check = Check::ReadAnotherAccount;
    let me = std::env::var("HOME").unwrap_or_default();
    let mut targets: Vec<PathBuf> = ["/root", "/var/root"]
        .iter()
        .map(PathBuf::from)
        .filter(|p| p.exists())
        .collect();
    for base in ["/home", "/Users"] {
        if let Ok(entries) = std::fs::read_dir(base) {
            for entry in entries.flatten() {
                let path = entry.path();
                let name = entry.file_name();
                if path.is_dir()
                    && path.to_string_lossy() != me
                    && name != "Shared"
                    && targets.len() < 4
                {
                    targets.push(path);
                }
            }
        }
    }
    if targets.is_empty() {
        return not_run(
            check,
            Expected::Blocked,
            "ls <another account's home>",
            "no other account's home directory was found to try".into(),
        );
    }
    let script = targets
        .iter()
        .map(|t| {
            format!(
                "ls \"{}\" >/dev/null 2>&1 && echo READ:{}",
                t.display(),
                t.display()
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    let output = match attempt(codex, lab, &script) {
        Ok(out) => out,
        Err(why) => return not_run(check, Expected::Blocked, &script, why),
    };
    let read: Vec<&str> = output
        .lines()
        .filter_map(|l| l.strip_prefix("READ:"))
        .collect();
    finding(
        check,
        Expected::Blocked,
        if read.is_empty() {
            Observed::Blocked
        } else {
            Observed::Allowed
        },
        &script,
        if read.is_empty() {
            format!(
                "none of {} could be listed",
                targets
                    .iter()
                    .map(|t| t.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        } else {
            format!("readable from inside the sandbox: {}", read.join(", "))
        },
    )
}

async fn processes_left_behind(codex: &str, lab: &Lab) -> Finding {
    use crate::worker::{WorkerLimits, WorkerSpec, run_worker};

    let check = Check::LeaveProcessesBehind;
    // A unique duration doubles as a marker to find the process by afterwards.
    // Process ids inside the sandbox may belong to another pid namespace, so
    // `$!` is not something the host can look up.
    let marker = format!(
        "{}.{}",
        290 + (std::process::id() % 7),
        std::process::id() % 100_000
    );
    let script = format!("echo ran > supervision-ran; sleep {marker} >/dev/null 2>&1 & exit 0");
    let args = sandboxed(codex, &script);
    let output_dir = lab.root.join("supervision");

    let spec = WorkerSpec {
        program: PathBuf::from(&args[0]),
        args: args[1..].iter().map(Into::into).collect(),
        cwd: Some(lab.worktree.clone()),
        env_set: Vec::new(),
        env_remove: Vec::new(),
        // Never read; the worker refuses an empty task.
        task: "probe".to_string(),
        output_dir,
        limits: WorkerLimits::with_deadline(std::time::Duration::from_secs(60)),
    };
    if let Err(why) = run_worker(&spec, std::future::pending::<()>()).await {
        return not_run(
            check,
            Expected::Blocked,
            &script,
            format!("the supervised worker did not run: {why}"),
        );
    }
    if !lab.worktree.join("supervision-ran").exists() {
        return not_run(
            check,
            Expected::Blocked,
            &script,
            "the supervised command never ran, so there was no background process to look for"
                .into(),
        );
    }
    // A moment for the kernel to finish reaping the group.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let survivors = surviving(&format!("sleep {marker}"));
    for pid in &survivors {
        // Clean up what the probe left, whatever the verdict.
        let _ = Command::new("kill").arg(pid.to_string()).status();
    }
    finding(
        check,
        Expected::Blocked,
        if survivors.is_empty() {
            Observed::Blocked
        } else {
            Observed::Allowed
        },
        &script,
        if survivors.is_empty() {
            "the background process was gone once Timon's worker supervision returned".into()
        } else {
            format!("still running after the worker ended: pid(s) {survivors:?}")
        },
    )
}

/// Pids whose command line is exactly `needle` or starts with it.
pub fn surviving(needle: &str) -> Vec<u32> {
    let Ok(output) = Command::new("ps").args(["-axo", "pid=,command="]).output() else {
        return Vec::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            let (pid, command) = line.split_once(char::is_whitespace)?;
            let command = command.trim();
            (command == needle || command.starts_with(&format!("{needle} ")))
                .then(|| pid.parse().ok())
                .flatten()
        })
        .collect()
}

/// The record `timon qualify` reads, with this run appended to any earlier ones
/// for the same host and Codex version. Earlier runs are kept, failures included.
pub fn record(
    existing: Option<serde_json::Value>,
    host: &str,
    codex_version: &str,
    run: &Run,
) -> serde_json::Value {
    let mut runs = existing
        .as_ref()
        .filter(|v| v["host"] == host && v["codex"] == codex_version)
        .and_then(|v| v["runs"].as_array().cloned())
        .unwrap_or_default();
    runs.push(serde_json::to_value(run).unwrap_or_default());
    serde_json::json!({
        "host": host,
        "codex": codex_version,
        "sandbox_mode": "workspace-write, network off",
        "how": "timon qualify probe: each check attempted through `codex sandbox` with the \
                settings a writing worker gets, from a worktree of a throwaway repository, \
                and read back from the filesystem, repository or process table afterwards",
        "runs": runs,
    })
}
