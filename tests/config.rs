//! The settings a developer should not have to retype.
//!
//! Two properties carry the weight. A misspelled key must say so rather than
//! being ignored, because the alternative is someone wondering why their
//! account setting did nothing. And nothing in a config file may authorise
//! spending: `--execute` stays a flag you type.

use std::io::Write;

use timon::config::{Settings, expand, load_from, starter};

fn write(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
    let file = dir.join("config.toml");
    let mut handle = std::fs::File::create(&file).unwrap();
    handle.write_all(body.as_bytes()).unwrap();
    file
}

#[test]
fn an_absent_config_is_not_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let settings = load_from(&dir.path().join("nothing-here.toml")).expect("absent is fine");
    assert!(settings.accounts.is_none());
    assert!(settings.strong_model.is_none());
}

#[test]
fn settings_are_read() {
    let dir = tempfile::tempdir().unwrap();
    let file = write(
        dir.path(),
        "accounts = [\"acct3\"]\nstrong_model = \"big\"\ncheap_model = \"small\"\nbudget_secs = 600\nallow_planner = true\n",
    );
    let settings = load_from(&file).expect("valid");
    assert_eq!(
        settings.accounts.as_deref(),
        Some(&["acct3".to_string()][..])
    );
    assert_eq!(settings.strong_model.as_deref(), Some("big"));
    assert_eq!(settings.cheap_model.as_deref(), Some("small"));
    assert_eq!(settings.budget_secs, Some(600));
    assert_eq!(settings.allow_planner, Some(true));
}

/// The property that matters most. A typo that fell back to a default would
/// leave the developer debugging the wrong thing entirely.
#[test]
fn a_misspelled_key_is_refused_rather_than_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let file = write(dir.path(), "acounts = [\"acct3\"]\n");
    let error = load_from(&file).expect_err("a typo must not be silently ignored");
    assert!(
        error.contains("acounts") || error.contains("unknown field"),
        "the error should name the bad key, got: {error}"
    );
    assert!(
        error.contains("timon config init"),
        "the error should say how to recover, got: {error}"
    );
}

#[test]
fn malformed_toml_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let file = write(dir.path(), "accounts = [unclosed\n");
    assert!(
        load_from(&file).is_err(),
        "broken syntax must stop the command"
    );
}

/// Nothing in the config may turn on spending. If a key for it ever appears,
/// `deny_unknown_fields` means this test fails rather than the property
/// quietly lapsing.
#[test]
fn no_config_key_can_authorise_spending() {
    let dir = tempfile::tempdir().unwrap();
    for key in ["execute", "execute_by_default", "auto_execute", "spend"] {
        let file = write(dir.path(), &format!("{key} = true\n"));
        assert!(
            load_from(&file).is_err(),
            "{key} must not be a recognised setting: paying for a run should \
             stay a flag somebody types"
        );
    }
}

#[test]
fn the_starter_file_parses_and_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let file = write(dir.path(), &starter());
    let settings = load_from(&file).expect("the file we ship must be valid");
    // Every line is commented out, so writing it must not alter behaviour.
    assert!(settings.accounts.is_none());
    assert!(settings.strong_model.is_none());
    assert!(settings.budget_secs.is_none());
    assert!(settings.allow_planner.is_none());
}

#[test]
fn the_starter_file_mentions_that_execute_is_still_needed() {
    assert!(
        starter().contains("--execute"),
        "someone reading the config should learn that it cannot spend on its own"
    );
}

#[test]
fn a_leading_tilde_is_expanded() {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    assert_eq!(
        expand(std::path::Path::new("~/work/thing")),
        std::path::PathBuf::from(&home).join("work/thing")
    );
    // Absolute and relative paths are left exactly as written.
    assert_eq!(
        expand(std::path::Path::new("/var/lib/timon")),
        std::path::PathBuf::from("/var/lib/timon")
    );
    assert_eq!(
        expand(std::path::Path::new("./runs")),
        std::path::PathBuf::from("./runs")
    );
}

#[test]
fn output_defaults_somewhere_durable_not_tmp() {
    // The old default was the system temp directory. /tmp was cleared under a
    // measurement run on this project and took every transcript with it.
    let root = timon::config::default_output_root();
    assert!(
        !root.starts_with("/tmp"),
        "run output must not default under /tmp, got {}",
        root.display()
    );
    assert!(root.is_absolute(), "got {}", root.display());
}

#[test]
fn an_empty_settings_struct_is_all_defaults() {
    let settings = Settings::default();
    assert!(settings.accounts.is_none());
    assert!(settings.broker.is_none());
    assert!(settings.output_root.is_none());
}
