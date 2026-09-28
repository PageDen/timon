// Adapted for Timon. Not derived from Prodex source.
//! The hand-off tool: how a developer inside a Codex session gives Timon work.
//!
//! P1.2. The other entry point is `timon run` in a terminal, and both record the
//! same run, because a hand-off should not mean something different depending on
//! where it was typed.
//!
//! **This path was qualified before it was built**, because the plan depended on
//! an assumption nobody had checked: that a Codex session can call a Timon tool
//! while its sandbox stays on. What testing found, against Codex 0.155.1:
//!
//! - Through the **app-server** protocol, which is what Desktop and the IDE
//!   extensions speak, a tool call with `sandbox: read-only` and
//!   `approvalPolicy: on-request` **is executed**. The server asked the client
//!   nothing about it. The app-server protocol has no MCP-approval request at
//!   all — only command execution, file changes, permissions and elicitation.
//! - In the **interactive CLI**, the client has its own approval UI, including
//!   "allow for this session" and "allow and don't ask again".
//! - In **`codex exec`** it can never work. Exec pins the approval policy to
//!   `never` whatever the configuration says, and an MCP call that needs
//!   approval is refused rather than asked about. This is a property of exec,
//!   not a setting to find.
//!
//! So the tool is for interactive sessions, and `timon run` is what automation
//! uses. Saying that here means nobody has to rediscover it.

use serde::Deserialize;
use serde_json::{Value, json};

use crate::run::record::Runs;
use crate::run::start::{Request, admit};

/// What the session asked for.
///
/// Everything except the goal is optional, because a developer mid-conversation
/// should be able to say what they want without filling in a form. Defaults come
/// from the server, which the developer started, not from the model.
#[derive(Debug, Deserialize)]
pub struct HandoffRequest {
    pub goal: String,
    #[serde(default)]
    pub workspace: Option<String>,
    #[serde(default)]
    pub accounts: Vec<String>,
    #[serde(default)]
    pub max_attempts: Option<u32>,
    #[serde(default)]
    pub token_ceiling: Option<u64>,
    #[serde(default)]
    pub deadline_secs: Option<i64>,
    /// Lets a session that lost its reply resubmit without starting a second run.
    #[serde(default)]
    pub submission_key: Option<String>,
}

/// Fixed by whoever started the server, never by the model.
///
/// The same reasoning as the delegation tool's policy: a model that could choose
/// its own limits would not be bounded by them.
#[derive(Clone, Debug)]
pub struct HandoffPolicy {
    /// Where runs are recorded.
    pub store: std::path::PathBuf,
    /// The repository this server was started for.
    ///
    /// **Authoritative, not a default.** A session may name a directory inside
    /// it — narrowing is safe — but not one outside it. The model chooses the
    /// workspace only in the sense that it chooses what the run reads now and
    /// writes later, which is the kind of choice P1.4 says caller-supplied
    /// metadata does not get to make. Found by testing: a live session quietly
    /// redirected a hand-off to its own working directory.
    pub workspace_root: Option<std::path::PathBuf>,
    pub default_max_attempts: u32,
    /// A ceiling the developer cannot raise from inside a session.
    pub max_attempts_limit: u32,
    pub attempt_reserve: u64,
    /// The uid runs are attributed to: this process's own, from the kernel.
    pub principal_uid: u32,
}

/// The tool as the model sees it.
pub fn describe() -> Value {
    json!({
        "name": "timon_handoff",
        "description":
            "Hand a goal to Timon. Timon triages it, chooses the model and the account, \
             runs it under host limits, and returns a branch and a report for review. \
             Use this for work that should outlive this conversation, or that needs \
             more than one worker. Returns a run id to check later. Nothing is written \
             to the user's working files.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "goal": {
                    "type": "string",
                    "description": "What the run should achieve, in the user's terms."
                },
                "workspace": {
                    "type": "string",
                    "description": "Usually omit this. Defaults to the repository Timon was started for. \
                                    May only narrow to a path inside that repository; naming one outside \
                                    is refused."
                },
                "accounts": {
                    "type": "array", "items": { "type": "string" },
                    "description": "Pooled accounts this run may spend. Omit to let the broker choose."
                },
                "max_attempts": {
                    "type": "integer",
                    "description": "Attempts this run may start. Capped by the server."
                },
                "token_ceiling": {
                    "type": "integer",
                    "description": "Token admission ceiling. Decides whether to start more work; never caps what running work spends."
                },
                "deadline_secs": {
                    "type": "integer",
                    "description": "Seconds from now after which no further work is started."
                },
                "submission_key": {
                    "type": "string",
                    "description": "Resubmitting with the same key returns the same run instead of starting another."
                }
            },
            "required": ["goal"]
        }
    })
}

/// Decides which repository a hand-off may use.
///
/// The server's root wins. A request may narrow to a path inside it and may
/// omit the field entirely; it may not point somewhere else.
fn resolve_workspace(
    policy: &HandoffPolicy,
    asked: Option<&str>,
) -> Result<Option<std::path::PathBuf>, String> {
    let Some(asked) = asked else {
        return Ok(policy.workspace_root.clone());
    };
    let asked = std::path::PathBuf::from(asked);
    let Some(root) = &policy.workspace_root else {
        // Nothing was pinned, so there is nothing to contradict.
        return Ok(Some(asked));
    };
    // Resolved, because `..` and symlinks are how "inside" stops being inside.
    let canonical_root = root.canonicalize().unwrap_or_else(|_| root.clone());
    let canonical_asked = asked.canonicalize().unwrap_or_else(|_| asked.clone());
    if canonical_asked.starts_with(&canonical_root) {
        return Ok(Some(canonical_asked));
    }
    // Says how to recover, because a refusal a model cannot act on is a refusal
    // it will simply repeat. Observed: a live session retried the identical call
    // twice against an earlier version of this message, then gave up.
    Err(format!(
        "Timon did not start the run. This hand-off server serves {}, and {} is \
         outside it.\n\nCall timon_handoff again without the `workspace` field to \
         use {}. If the other directory is really the one wanted, Timon has to be \
         started there: a session cannot redirect a hand-off on its own.",
        canonical_root.display(),
        canonical_asked.display(),
        canonical_root.display()
    ))
}

/// Records the hand-off, or explains why it was refused.
///
/// Returns the text the session sees and whether it is an error. A refusal comes
/// back as a tool result rather than a protocol error, so the model can relay the
/// reason to the developer instead of reporting that the tool broke.
pub fn handoff(policy: &HandoffPolicy, request: HandoffRequest, now: i64) -> (String, bool) {
    if request.goal.trim().is_empty() {
        return (
            "The goal was empty. Say what the run should achieve.".to_string(),
            true,
        );
    }

    let runs = match Runs::open(&policy.store) {
        Ok(runs) => runs,
        Err(error) => {
            return (format!("Timon could not open its run store: {error}"), true);
        }
    };

    let workspace = match resolve_workspace(policy, request.workspace.as_deref()) {
        Ok(workspace) => workspace,
        Err(why) => return (why, true),
    };

    // The developer may lower the attempt limit but not raise it past what the
    // server was started with. Limits a session can widen are not limits.
    let max_attempts = request
        .max_attempts
        .unwrap_or(policy.default_max_attempts)
        .min(policy.max_attempts_limit);

    let base = crate::run::start::base_of(workspace.as_deref());

    let admitted = admit(
        &runs,
        Request {
            goal: request.goal,
            principal_uid: policy.principal_uid,
            workspace,
            accounts: request.accounts,
            max_attempts,
            token_ceiling: request.token_ceiling,
            deadline: request.deadline_secs.map(|secs| now + secs),
            submission_key: request.submission_key,
        },
        base,
        now,
        None,
        policy.attempt_reserve,
    );

    match admitted {
        Ok(run) => (
            format!(
                "Handed off as {id}.\n\n\
                 goal      {goal}\n\
                 base      {base}\n\
                 attempts  up to {attempts}\n\n\
                 The run outlives this conversation. `timon runs show {id}` reports it, \
                 and `timon runs cancel {id}` stops it. Nothing has been written to the \
                 working files, and nothing reaches them until the resulting branch is \
                 reviewed.\n\n\
                 Triage and the pipeline are not built yet, so no model has been called \
                 and no quota has been spent for this run.",
                id = run.id,
                goal = run.goal,
                base = run.base.describe(),
                attempts = run.max_attempts,
            ),
            false,
        ),
        Err(refused) => (format!("Timon did not start the run. {refused}"), true),
    }
}
