//! The settings a developer should not have to retype.
//!
//! Running a goal needs an account, two model names, a budget, somewhere to put
//! the output and the broker's address. Typing all of that every time is how a
//! tool stops being used, so it lives in a file and the flags override it.
//!
//! Three rules, in order of how much they matter:
//!
//! **A malformed config is an error, not a shrug.** A typo in a key name would
//! otherwise fall back to a default and the developer would be left wondering
//! why their account setting did nothing. Absent is fine; present and broken
//! stops the command. `deny_unknown_fields` is what makes a misspelled key say
//! so rather than being ignored.
//!
//! **Nothing here can authorise spending.** `--execute` stays a flag you type.
//! Recording a run and paying for one should not be the same keystroke, and a
//! config file that could flip that would make it one.
//!
//! **Output goes somewhere durable.** The old default was the system temp
//! directory, which cost this project a whole measurement run's transcripts
//! when `/tmp` was cleared underneath it.

use std::num::NonZeroU16;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// What a config file may set. Every field is optional: a file that sets one
/// thing and leaves the rest alone is the normal case.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// Pooled accounts a run may spend. Omitted lets the broker choose.
    pub accounts: Option<Vec<String>>,
    pub cheap_model: Option<String>,
    pub strong_model: Option<String>,
    pub broker: Option<String>,
    pub budget_secs: Option<u64>,
    pub concurrency: Option<usize>,
    /// Whether a goal may reach the planner path at all.
    pub allow_planner: Option<bool>,
    pub output_root: Option<PathBuf>,
    pub store: Option<PathBuf>,
    pub slot_dir: Option<PathBuf>,
    pub slots: Option<NonZeroU16>,
    pub slots_per_user: Option<NonZeroU16>,
}

fn home() -> String {
    std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string())
}

/// Where Timon keeps per-developer state.
///
/// `TIMON_HOME` so a second setup, or a test, can be pointed elsewhere without
/// disturbing the real one.
pub fn timon_home() -> PathBuf {
    match std::env::var_os("TIMON_HOME") {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(home()).join(".timon"),
    }
}

/// Where the config lives.
pub fn path() -> PathBuf {
    if let Ok(explicit) = std::env::var("TIMON_CONFIG")
        && !explicit.is_empty()
    {
        return PathBuf::from(explicit);
    }
    config_home().join("timon").join("config.toml")
}

fn config_home() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME")
        && !xdg.is_empty()
    {
        return PathBuf::from(xdg);
    }
    PathBuf::from(home()).join(".config")
}

/// Reads the config, or returns an empty one if there is no file.
///
/// A file that exists but does not parse is an error naming the problem, because
/// the alternative is a developer whose `account` setting silently did nothing.
pub fn load() -> Result<Settings, String> {
    load_from(&path())
}

/// The same, from a stated path. Separated so the rules are testable.
pub fn load_from(file: &Path) -> Result<Settings, String> {
    let raw = match std::fs::read_to_string(file) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Settings::default());
        }
        Err(error) => return Err(format!("reading {}: {error}", file.display())),
    };
    toml::from_str(&raw).map_err(|error| {
        format!(
            "{} is not valid: {error}\nFix it, or move it aside and run \
             `timon config init` for a fresh one.",
            file.display()
        )
    })
}

/// Where run output goes when nothing says otherwise.
///
/// Durable, under the caller's own data directory. The previous default was the
/// system temp directory; `/tmp` was cleared under a measurement run and took
/// every transcript with it. Those transcripts are also the only record of what
/// a worker actually did, which is what anyone reviewing a hand-off needs.
pub fn default_output_root() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME")
        && !xdg.is_empty()
    {
        return PathBuf::from(xdg).join("timon").join("runs");
    }
    PathBuf::from(home())
        .join(".local")
        .join("share")
        .join("timon")
        .join("runs")
}

/// Expands a leading `~`, so a config file can say `~/work` and mean it.
pub fn expand(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix("~/") {
        return PathBuf::from(home()).join(rest);
    }
    if text == "~" {
        return PathBuf::from(home());
    }
    path.to_path_buf()
}

/// The file `timon config init` writes.
///
/// Commented rather than blank: a developer reading it should be able to tell
/// what each setting does without the help output, and every line is commented
/// out so writing the file changes no behaviour until something is uncommented.
pub fn starter() -> String {
    "\
# Timon configuration. Command-line flags override everything here.
#
#   timon config show    what is actually in effect, and where it came from
#   timon status         the broker, the accounts and recent runs
#
# Nothing in this file can authorise spending. `timon run` still needs
# --execute before it will pay for anything.

# Pooled accounts a run may spend. Omit to let the broker choose by headroom.
# accounts = [\"acct3\"]

# The two models. Timon triages the goal to pick between them; the planner and
# the verifier always use the strong one.
# strong_model = \"gpt-5.6-luna\"
# cheap_model = \"gpt-5.5\"

# How long a whole run may take, in seconds. The worker deadline, the point at
# which new work stops and the depth a plan may reach are all derived from it,
# so the worst case is this number rather than something larger. Because it is
# divided up, a small total gives each worker very little: 300 here means 60
# seconds per worker, which is not enough to write a document.
# budget_secs = 900

# Allow the planner path, which splits a goal into a task graph. Without it the
# most that happens is a single strong call.
# allow_planner = true

# Workers at once.
# concurrency = 4

# Where the broker listens. It holds the credentials; nothing else should.
# broker = \"127.0.0.1:1456\"

# Where worker output and transcripts are kept. Durable on purpose: this is the
# only record of what a worker did.
# output_root = \"~/.local/share/timon/runs\"

# The run store.
# store = \"~/.timon/runs.sqlite\"

# Host-wide worker slots, for a shared machine. `slots_per_user` stops one
# person's work occupying every slot.
# slot_dir = \"/var/lib/timon/slots\"
# slots = 8
# slots_per_user = 3
"
    .to_string()
}

/// This host's name.
///
/// From `gethostname(2)` rather than `/proc/sys/kernel/hostname`, which exists
/// only on Linux: on a Mac that read returned nothing, so a qualification
/// recorded there would have named no host at all, and the check that a record
/// belongs to this machine would have compared two empty strings.
pub fn hostname() -> String {
    let mut buffer = [0u8; 256];
    // SAFETY: the buffer is valid for its whole length, and gethostname writes
    // at most that many bytes.
    let result = unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) };
    if result != 0 {
        return String::new();
    }
    let end = buffer.iter().position(|&b| b == 0).unwrap_or(buffer.len());
    String::from_utf8_lossy(&buffer[..end]).trim().to_string()
}
