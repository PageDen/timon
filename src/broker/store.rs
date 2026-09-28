// Adapted for Timon. Not derived from Prodex source.
//! The pooled account store: what is in it, and whether it is safe.

use std::path::{Path, PathBuf};

use serde::Serialize;

/// The file `codex login` writes, which is what an account amounts to here.
pub const CREDENTIAL_FILE: &str = "auth.json";

/// How much of an account identifier is shown.
///
/// Enough to tell two pooled accounts apart in a report, and not the whole value.
/// An account id is not a credential, but it is an identifier belonging to a
/// person, and a short prefix does the operator's job without echoing it.
const ID_PREFIX: usize = 8;

/// What is known about one pooled account.
///
/// Carries no token, by construction. There is no field here that could
/// authenticate a request, so no rendering of this type can leak one.
#[derive(Clone, Debug, Serialize)]
pub struct Account {
    pub name: String,
    /// Directory to hand to the child as `CODEX_HOME`.
    pub home: PathBuf,
    /// `chatgpt` or `apikey`, as the credential file reports it.
    pub auth_mode: Option<String>,
    /// First few characters of the provider account id, to distinguish accounts.
    pub account_id_prefix: Option<String>,
    /// Whether a refresh token is present, which decides whether this account can
    /// outlive its current access token.
    pub refreshable: bool,
    /// What the file says about its last refresh, verbatim.
    pub last_refresh: Option<String>,
    /// Problems that make this account unusable or unsafe, in operator language.
    pub faults: Vec<String>,
}

impl Account {
    /// True when nothing about this account would stop it serving a request.
    pub fn usable(&self) -> bool {
        self.faults.is_empty()
    }
}

/// Why a store could not be read at all.
#[derive(Debug)]
pub enum StoreError {
    Missing(PathBuf),
    NotADirectory(PathBuf),
    Io(PathBuf, std::io::Error),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Missing(path) => write!(f, "{} does not exist", path.display()),
            StoreError::NotADirectory(path) => write!(f, "{} is not a directory", path.display()),
            StoreError::Io(path, error) => write!(f, "{}: {error}", path.display()),
        }
    }
}

impl std::error::Error for StoreError {}

/// A directory of pooled accounts, one subdirectory each.
#[derive(Debug)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, StoreError> {
        let root = root.into();
        if !root.exists() {
            return Err(StoreError::Missing(root));
        }
        if !root.is_dir() {
            return Err(StoreError::NotADirectory(root));
        }
        Ok(Store { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Every account in the store, named alphabetically so output is stable.
    ///
    /// One account's problems never hide another's: a directory whose credential
    /// file is missing or unreadable is returned with its faults listed rather
    /// than omitted, because an operator needs to see the broken one.
    pub fn accounts(&self) -> Result<Vec<Account>, StoreError> {
        let mut names: Vec<String> = std::fs::read_dir(&self.root)
            .map_err(|error| StoreError::Io(self.root.clone(), error))?
            .flatten()
            .filter(|entry| entry.path().is_dir())
            .filter_map(|entry| entry.file_name().to_str().map(str::to_string))
            .filter(|name| !name.starts_with('.'))
            .collect();
        names.sort();
        Ok(names.iter().map(|name| self.read(name)).collect())
    }

    /// Reads one account, reporting faults rather than failing.
    pub fn read(&self, name: &str) -> Account {
        let home = self.root.join(name);
        let credential = home.join(CREDENTIAL_FILE);
        let mut account = Account {
            name: name.to_string(),
            home: home.clone(),
            auth_mode: None,
            account_id_prefix: None,
            refreshable: false,
            last_refresh: None,
            faults: Vec::new(),
        };

        // Permissions first: an account whose credential others can read is a
        // fault whatever else is true of it, and it is the fault this whole
        // design exists to prevent.
        account.faults.extend(exposure_faults(&home, &credential));

        let text = match std::fs::read_to_string(&credential) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                account.faults.push(format!(
                    "no {CREDENTIAL_FILE}: this account has never been logged in"
                ));
                return account;
            }
            Err(error) => {
                // Deliberately not including the error's own rendering of the
                // path contents; only why it could not be read.
                account.faults.push(format!(
                    "{CREDENTIAL_FILE} could not be read: {}",
                    error.kind()
                ));
                return account;
            }
        };

        let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&text) else {
            account
                .faults
                .push(format!("{CREDENTIAL_FILE} is not valid JSON"));
            return account;
        };

        account.auth_mode = parsed
            .get("auth_mode")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        account.last_refresh = parsed
            .get("last_refresh")
            .and_then(|v| v.as_str())
            .map(str::to_string);

        let tokens = parsed.get("tokens");
        account.refreshable = tokens
            .and_then(|t| t.get("refresh_token"))
            .and_then(|v| v.as_str())
            .is_some_and(|value| !value.is_empty());
        account.account_id_prefix = tokens
            .and_then(|t| t.get("account_id"))
            .and_then(|v| v.as_str())
            .map(|id| {
                let prefix: String = id.chars().take(ID_PREFIX).collect();
                format!("{prefix}…")
            });

        let has_access = tokens
            .and_then(|t| t.get("access_token"))
            .and_then(|v| v.as_str())
            .is_some_and(|value| !value.is_empty());
        let has_api_key = parsed
            .get("OPENAI_API_KEY")
            .and_then(|v| v.as_str())
            .is_some_and(|value| !value.is_empty());
        if !has_access && !has_api_key {
            account
                .faults
                .push("no usable credential: neither an access token nor an API key".to_string());
        }
        if account.auth_mode.as_deref() == Some("chatgpt") && !account.refreshable {
            // An access token without a refresh token works until it expires and
            // then fails in the middle of somebody's session.
            account.faults.push(
                "no refresh token: this account will stop working when its access token expires"
                    .to_string(),
            );
        }
        account
    }
}

/// Faults about who can read the credential.
///
/// The point of the store is that a user's own process cannot read what it
/// authenticates with. A group- or world-readable credential file silently undoes
/// that, so it is reported as a fault rather than a note.
#[cfg(unix)]
fn exposure_faults(home: &Path, credential: &Path) -> Vec<String> {
    use std::os::unix::fs::PermissionsExt;
    let mut faults = Vec::new();
    for (path, what) in [(home, "directory"), (credential, "credential file")] {
        let Ok(meta) = std::fs::metadata(path) else {
            continue;
        };
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            faults.push(format!(
                "the {what} is readable beyond its owner (mode {mode:04o}): \
                 a pooled credential others can read defeats the point of pooling it"
            ));
        }
    }
    faults
}

#[cfg(not(unix))]
fn exposure_faults(_home: &Path, _credential: &Path) -> Vec<String> {
    Vec::new()
}

/// What an operator sees when asking what is in the store.
#[derive(Debug, Serialize)]
pub struct Inventory {
    pub root: PathBuf,
    pub accounts: Vec<Account>,
    /// Accounts that could serve a request right now.
    pub usable: usize,
    /// Rotation needs more than one usable account to mean anything.
    pub rotation_possible: bool,
}

impl Inventory {
    pub fn of(store: &Store) -> Result<Self, StoreError> {
        let accounts = store.accounts()?;
        let usable = accounts.iter().filter(|a| a.usable()).count();
        Ok(Inventory {
            root: store.root().to_path_buf(),
            accounts,
            usable,
            rotation_possible: usable > 1,
        })
    }
}

/// Renders an inventory for a terminal.
pub fn render(inventory: &Inventory) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "Pooled accounts in {}\n\n",
        inventory.root.display()
    ));
    if inventory.accounts.is_empty() {
        out.push_str("  none: no account directory in this store\n");
    }
    for account in &inventory.accounts {
        out.push_str(&format!(
            "  {} {}\n",
            if account.usable() { "ok  " } else { "FAULT" },
            account.name
        ));
        out.push_str(&format!(
            "        mode {}  account {}  refreshable {}\n",
            account.auth_mode.as_deref().unwrap_or("unknown"),
            account.account_id_prefix.as_deref().unwrap_or("unknown"),
            account.refreshable
        ));
        if let Some(when) = &account.last_refresh {
            out.push_str(&format!("        last refresh {when}\n"));
        }
        for fault in &account.faults {
            out.push_str(&format!("        - {fault}\n"));
        }
    }
    out.push_str(&format!(
        "\n{} usable, rotation {}\n",
        inventory.usable,
        if inventory.rotation_possible {
            "possible"
        } else {
            "not possible: it needs more than one usable account"
        }
    ));
    out.push_str(
        "No credential value is shown here, or anywhere else this store is read \
         from. To add an account, log in with CODEX_HOME set to a new \
         subdirectory of this store, as the account that owns it.\n",
    );
    out
}

/// The credential an account authenticates with.
///
/// Read only when a request is about to be served, held in memory, and never
/// rendered: this type has no `Debug`, no `Serialize` and no `Display`, so there
/// is no formatting path that can print it by accident.
pub struct Credential {
    pub bearer: String,
    pub account_id: Option<String>,
}

/// Why a credential could not be read.
#[derive(Debug)]
pub enum CredentialError {
    Unreadable(std::io::ErrorKind),
    NotJson,
    /// Neither an access token nor an API key, so there is nothing to send.
    Absent,
}

impl std::fmt::Display for CredentialError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CredentialError::Unreadable(kind) => {
                write!(f, "{CREDENTIAL_FILE} could not be read: {kind}")
            }
            CredentialError::NotJson => write!(f, "{CREDENTIAL_FILE} is not valid JSON"),
            CredentialError::Absent => write!(
                f,
                "no usable credential: neither an access token nor an API key"
            ),
        }
    }
}

impl std::error::Error for CredentialError {}

/// Reads the bearer an account should authenticate with.
///
/// Prefers the OAuth access token, falling back to an API key for an `apikey`
/// account. Refreshing an expired token is a later slice; this returns whatever
/// is stored.
pub fn credential_of(account: &Account) -> Result<Credential, CredentialError> {
    let text = std::fs::read_to_string(account.home.join(CREDENTIAL_FILE))
        .map_err(|error| CredentialError::Unreadable(error.kind()))?;
    let parsed: serde_json::Value =
        serde_json::from_str(&text).map_err(|_| CredentialError::NotJson)?;
    let tokens = parsed.get("tokens");
    let bearer = tokens
        .and_then(|t| t.get("access_token"))
        .and_then(|v| v.as_str())
        .filter(|value| !value.is_empty())
        .or_else(|| {
            parsed
                .get("OPENAI_API_KEY")
                .and_then(|v| v.as_str())
                .filter(|value| !value.is_empty())
        })
        .ok_or(CredentialError::Absent)?
        .to_string();
    let account_id = tokens
        .and_then(|t| t.get("account_id"))
        .and_then(|v| v.as_str())
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    Ok(Credential { bearer, account_id })
}
