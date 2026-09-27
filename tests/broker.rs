//! The pooled account store (amendment A4, slice 1).
//!
//! One invariant matters more than the rest and most of these tests are about it:
//! **a credential value never leaves the store.** The types carry no token, so
//! there should be nothing to leak — and these tests assert that by looking for
//! the token's own text in every rendering, rather than by trusting the design.

use serde_json::json;
use timon::broker::store::{Inventory, Store, render};

/// Distinctive strings, so finding one in any output is unambiguous.
const ACCESS: &str = "ACCESS-TOKEN-MUST-NEVER-APPEAR-a1b2c3";
const REFRESH: &str = "REFRESH-TOKEN-MUST-NEVER-APPEAR-d4e5f6";
const ID_TOKEN: &str = "ID-TOKEN-MUST-NEVER-APPEAR-g7h8i9";
const API_KEY: &str = "sk-API-KEY-MUST-NEVER-APPEAR-j1k2l3";

fn account(root: &std::path::Path, name: &str, body: serde_json::Value) -> std::path::PathBuf {
    let home = root.join(name);
    std::fs::create_dir_all(&home).unwrap();
    let path = home.join("auth.json");
    std::fs::write(&path, serde_json::to_string_pretty(&body).unwrap()).unwrap();
    restrict(&home, 0o700);
    restrict(&path, 0o600);
    home
}

fn restrict(path: &std::path::Path, mode: u32) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }
}

fn chatgpt(id: &str) -> serde_json::Value {
    json!({
        "auth_mode": "chatgpt",
        "OPENAI_API_KEY": null,
        "tokens": {
            "id_token": ID_TOKEN,
            "access_token": ACCESS,
            "refresh_token": REFRESH,
            "account_id": id
        },
        "last_refresh": "2026-09-27T12:00:00.000000Z"
    })
}

#[test]
fn a_well_formed_store_lists_its_accounts() {
    let dir = tempfile::tempdir().unwrap();
    account(dir.path(), "alpha", chatgpt("acct-alpha-0000000000"));
    account(dir.path(), "beta", chatgpt("acct-beta-11111111111"));

    let store = Store::open(dir.path()).unwrap();
    let inventory = Inventory::of(&store).unwrap();

    assert_eq!(inventory.accounts.len(), 2);
    assert_eq!(inventory.usable, 2);
    assert!(
        inventory.rotation_possible,
        "two usable accounts can rotate"
    );
    // Alphabetical, so output is stable between runs.
    assert_eq!(inventory.accounts[0].name, "alpha");
    assert_eq!(inventory.accounts[1].name, "beta");
    assert_eq!(inventory.accounts[0].auth_mode.as_deref(), Some("chatgpt"));
    assert!(inventory.accounts[0].refreshable);
}

#[test]
fn no_credential_value_appears_in_any_rendering() {
    // The invariant. Asserted against the token's own text rather than inferred
    // from the types, because a future field could reintroduce one silently.
    let dir = tempfile::tempdir().unwrap();
    account(dir.path(), "alpha", chatgpt("acct-alpha-0000000000"));
    account(
        dir.path(),
        "keyed",
        json!({"auth_mode": "apikey", "OPENAI_API_KEY": API_KEY, "tokens": null}),
    );

    let store = Store::open(dir.path()).unwrap();
    let inventory = Inventory::of(&store).unwrap();
    let text = render(&inventory);
    let as_json = serde_json::to_string(&inventory).unwrap();
    let debug = format!("{inventory:?}");

    for secret in [ACCESS, REFRESH, ID_TOKEN, API_KEY] {
        for (what, rendered) in [("text", &text), ("json", &as_json), ("debug", &debug)] {
            assert!(
                !rendered.contains(secret),
                "{what} rendering leaked a credential value"
            );
        }
    }
}

#[test]
fn the_account_identifier_is_shortened_rather_than_echoed() {
    let dir = tempfile::tempdir().unwrap();
    let full = "acct-alpha-0000000000";
    account(dir.path(), "alpha", chatgpt(full));

    let store = Store::open(dir.path()).unwrap();
    let inventory = Inventory::of(&store).unwrap();
    let prefix = inventory.accounts[0].account_id_prefix.clone().unwrap();

    assert!(prefix.starts_with("acct-alp"), "got {prefix}");
    assert!(prefix.ends_with('…'));
    assert!(
        !render(&inventory).contains(full),
        "the whole identifier must not be printed"
    );
}

#[test]
fn one_broken_account_does_not_hide_the_others() {
    let dir = tempfile::tempdir().unwrap();
    account(dir.path(), "good", chatgpt("acct-good-000000000000"));
    // Present but unreadable as JSON.
    let broken = dir.path().join("broken");
    std::fs::create_dir_all(&broken).unwrap();
    std::fs::write(broken.join("auth.json"), "{ not json").unwrap();
    restrict(&broken, 0o700);
    restrict(&broken.join("auth.json"), 0o600);
    // Present but never logged in.
    let empty = dir.path().join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    restrict(&empty, 0o700);

    let store = Store::open(dir.path()).unwrap();
    let inventory = Inventory::of(&store).unwrap();

    assert_eq!(
        inventory.accounts.len(),
        3,
        "the broken ones are listed too"
    );
    assert_eq!(inventory.usable, 1);
    assert!(
        !inventory.rotation_possible,
        "one usable account cannot rotate"
    );

    let by_name = |n: &str| {
        inventory
            .accounts
            .iter()
            .find(|a| a.name == n)
            .unwrap()
            .clone()
    };
    assert!(by_name("good").usable());
    assert!(
        by_name("broken")
            .faults
            .iter()
            .any(|f| f.contains("not valid JSON"))
    );
    assert!(
        by_name("empty")
            .faults
            .iter()
            .any(|f| f.contains("never been logged in"))
    );
}

#[test]
fn a_credential_others_can_read_is_a_fault() {
    // The reason the store exists is that a user cannot read what authenticates
    // their request. A group-readable file undoes that silently, so it is a fault
    // rather than a note.
    let dir = tempfile::tempdir().unwrap();
    let home = account(dir.path(), "leaky", chatgpt("acct-leaky-00000000000"));
    restrict(&home.join("auth.json"), 0o644);

    let store = Store::open(dir.path()).unwrap();
    let inventory = Inventory::of(&store).unwrap();
    let leaky = &inventory.accounts[0];

    assert!(!leaky.usable());
    assert!(
        leaky
            .faults
            .iter()
            .any(|f| f.contains("readable beyond its owner")),
        "got {:?}",
        leaky.faults
    );
    assert_eq!(inventory.usable, 0);
}

#[test]
fn a_world_readable_account_directory_is_a_fault_too() {
    let dir = tempfile::tempdir().unwrap();
    let home = account(dir.path(), "open", chatgpt("acct-open-0000000000"));
    restrict(&home, 0o755);

    let store = Store::open(dir.path()).unwrap();
    let inventory = Inventory::of(&store).unwrap();
    assert!(
        inventory.accounts[0]
            .faults
            .iter()
            .any(|f| f.contains("directory is readable beyond its owner")),
        "got {:?}",
        inventory.accounts[0].faults
    );
}

#[test]
fn a_chatgpt_account_without_a_refresh_token_is_a_fault() {
    // It works until the access token expires and then fails in the middle of
    // somebody's session, which is the worst time to find out.
    let dir = tempfile::tempdir().unwrap();
    account(
        dir.path(),
        "stale",
        json!({
            "auth_mode": "chatgpt",
            "tokens": {"access_token": ACCESS, "account_id": "acct-stale-0000000000"},
            "last_refresh": "2026-09-01T00:00:00Z"
        }),
    );
    let store = Store::open(dir.path()).unwrap();
    let inventory = Inventory::of(&store).unwrap();
    assert!(!inventory.accounts[0].refreshable);
    assert!(
        inventory.accounts[0]
            .faults
            .iter()
            .any(|f| f.contains("no refresh token")),
        "got {:?}",
        inventory.accounts[0].faults
    );
}

#[test]
fn an_account_with_no_usable_credential_is_a_fault() {
    let dir = tempfile::tempdir().unwrap();
    account(
        dir.path(),
        "hollow",
        json!({"auth_mode": "chatgpt", "tokens": {"account_id": "acct-x"}}),
    );
    let store = Store::open(dir.path()).unwrap();
    let inventory = Inventory::of(&store).unwrap();
    assert!(
        inventory.accounts[0]
            .faults
            .iter()
            .any(|f| f.contains("no usable credential")),
        "got {:?}",
        inventory.accounts[0].faults
    );
}

#[test]
fn an_empty_store_says_so_rather_than_failing() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let inventory = Inventory::of(&store).unwrap();
    assert!(inventory.accounts.is_empty());
    assert!(!inventory.rotation_possible);
    assert!(render(&inventory).contains("none: no account directory"));
}

#[test]
fn a_missing_store_is_an_error_with_a_way_forward() {
    let dir = tempfile::tempdir().unwrap();
    let error = Store::open(dir.path().join("absent")).expect_err("a missing store is an error");
    assert!(format!("{error}").contains("does not exist"));
}

#[test]
fn dot_directories_are_not_accounts() {
    // A partially written account, or an editor's leftovers, should not be
    // mistaken for something that can serve a request.
    let dir = tempfile::tempdir().unwrap();
    account(dir.path(), "real", chatgpt("acct-real-0000000000"));
    std::fs::create_dir_all(dir.path().join(".partial")).unwrap();

    let store = Store::open(dir.path()).unwrap();
    let inventory = Inventory::of(&store).unwrap();
    assert_eq!(inventory.accounts.len(), 1);
    assert_eq!(inventory.accounts[0].name, "real");
}

#[test]
fn rotation_needs_more_than_one_usable_account() {
    // The arithmetic that closed this whole question once already: rotation
    // across one account does nothing.
    let dir = tempfile::tempdir().unwrap();
    account(dir.path(), "only", chatgpt("acct-only-0000000000"));
    let store = Store::open(dir.path()).unwrap();
    let inventory = Inventory::of(&store).unwrap();
    assert_eq!(inventory.usable, 1);
    assert!(!inventory.rotation_possible);
    assert!(render(&inventory).contains("not possible"));
}
