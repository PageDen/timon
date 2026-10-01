//! The command a worker runs, and what it does not leave to chance.
//!
//! Both properties here were found by reading a real run's transcript, not by
//! any earlier test. A worker given no provider talked straight to OpenAI on
//! the developer's own login, so the broker forwarded nothing and no run was
//! paid from the pooled accounts. A worker given no sandbox inherited the
//! developer's default, which on the host it was found on was
//! `danger-full-access`.

use std::ffi::OsString;

use timon::broker::grant::GRANT_HEADER;
use timon::run::execute::{GRANT_ENV, PROVIDER, Sandbox, provider_settings, worker_command};

fn strings(command: &[OsString]) -> Vec<String> {
    command
        .iter()
        .map(|part| part.to_string_lossy().into_owned())
        .collect()
}

/// The value following a flag, for every occurrence of it.
fn values_of(command: &[String], flag: &str) -> Vec<String> {
    command
        .windows(2)
        .filter(|pair| pair[0] == flag)
        .map(|pair| pair[1].clone())
        .collect()
}

#[test]
fn every_worker_states_its_sandbox() {
    for sandbox in [Sandbox::ReadOnly, Sandbox::WorkspaceWrite] {
        let command = strings(&worker_command("127.0.0.1:1456", Some("m"), sandbox));
        assert_eq!(
            values_of(&command, "-s"),
            vec![sandbox.as_str().to_string()],
            "exactly one sandbox, stated, in {command:?}"
        );
    }
}

#[test]
fn no_worker_is_ever_given_full_access() {
    for sandbox in [Sandbox::ReadOnly, Sandbox::WorkspaceWrite] {
        let command = strings(&worker_command("127.0.0.1:1456", None, sandbox)).join(" ");
        assert!(!command.contains("danger-full-access"), "got {command}");
        assert!(!command.contains("dangerously-bypass"), "got {command}");
    }
}

/// The finding that started this: the broker must be the provider, whatever
/// the developer's own Codex config says.
#[test]
fn the_broker_is_the_provider() {
    let command = strings(&worker_command("127.0.0.1:1456", None, Sandbox::ReadOnly));
    let settings = values_of(&command, "-c");
    assert!(
        settings.contains(&format!("model_provider=\"{PROVIDER}\"")),
        "got {settings:?}"
    );
    assert!(
        settings.contains(&format!(
            "model_providers.{PROVIDER}.base_url=\"http://127.0.0.1:1456\""
        )),
        "got {settings:?}"
    );
}

#[test]
fn the_broker_address_is_the_one_configured() {
    let settings = provider_settings("127.0.0.1:2456");
    assert!(
        settings
            .iter()
            .any(|s| s.contains("base_url=\"http://127.0.0.1:2456\"")),
        "a tunnelled or relocated broker must be honoured: {settings:?}"
    );
}

/// The grant reaches the broker as a header, from the environment variable the
/// executor sets. A mismatch here would be a worker that is routed correctly and
/// refused every time.
#[test]
fn the_grant_travels_as_the_header_the_broker_reads() {
    let settings = provider_settings("127.0.0.1:1456");
    let headers = settings
        .iter()
        .find(|s| s.contains("env_http_headers"))
        .expect("a header mapping");
    assert!(headers.contains(GRANT_HEADER), "got {headers}");
    assert!(headers.contains(GRANT_ENV), "got {headers}");
}

/// The task goes on stdin. Anything in the arguments is readable by every
/// account on the host through a process listing.
#[test]
fn the_task_is_never_in_the_arguments() {
    let command = strings(&worker_command(
        "127.0.0.1:1456",
        Some("m"),
        Sandbox::ReadOnly,
    ));
    assert_eq!(
        command.last().map(String::as_str),
        Some("-"),
        "got {command:?}"
    );
}

/// The settings must parse as TOML values, which is how Codex reads `-c`. A
/// value that failed to parse would be taken as a plain string and the provider
/// silently misconfigured.
#[test]
fn every_setting_is_valid_toml() {
    for setting in provider_settings("127.0.0.1:1456") {
        let (key, value) = setting.split_once('=').expect("key=value");
        let parsed: Result<toml::Value, _> = toml::from_str(&format!("v = {value}"));
        assert!(parsed.is_ok(), "{key} has an unparseable value: {value}");
    }
}

/// The qualification recorded `reach_git_remote: blocked`, while the host's own
/// Codex config enabled network for the write sandbox and workers inherited it.
/// The command pins it off so the state workers run in is the one qualified.
#[test]
fn workers_cannot_reach_the_network_whatever_the_config_says() {
    for sandbox in [Sandbox::ReadOnly, Sandbox::WorkspaceWrite] {
        let command = strings(&worker_command("127.0.0.1:1456", None, sandbox));
        assert!(
            values_of(&command, "-c")
                .contains(&"sandbox_workspace_write.network_access=false".to_string()),
            "got {command:?}"
        );
    }
}
