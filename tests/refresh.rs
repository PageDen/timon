//! Renewing a pooled account's access token.
//!
//! One rule dominates this file. A refresh token is **single-use and rotates**:
//! spending one yields a new one, and the old one dies. An account whose refresh
//! token is lost or overwritten with nothing cannot be renewed again and has to
//! be logged in by hand. Several tests here exist only to hold that line.

use serde_json::json;
use timon::broker::refresh::{apply, due, expires_at};

/// Builds a JWT-shaped token with the given expiry. Only the payload is read.
fn token(exp: i64) -> String {
    let payload = serde_json::to_vec(&json!({ "exp": exp })).unwrap();
    format!("eyJhbGciOiJub25lIn0.{}.signature", base64url(&payload))
}

fn base64url(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in input.chunks(3) {
        let mut buffer = 0u32;
        for (index, byte) in chunk.iter().enumerate() {
            buffer |= (*byte as u32) << (16 - 8 * index);
        }
        for index in 0..chunk.len() + 1 {
            out.push(TABLE[((buffer >> (18 - 6 * index)) & 0x3f) as usize] as char);
        }
    }
    out
}

fn stored() -> serde_json::Value {
    json!({
        "tokens": {
            "access_token": token(1_000_000_000),
            "refresh_token": "ORIGINAL-REFRESH-TOKEN",
            "id_token": "ORIGINAL-ID-TOKEN",
            "account_id": "account-id-1",
        },
        "last_refresh": "2026-09-01T00:00:00Z",
    })
}

#[test]
fn a_reply_without_a_refresh_token_leaves_the_existing_one_alone() {
    // The one that matters. Writing `null` here would strand the account.
    let mut credential = stored();
    let granted = json!({ "access_token": token(2_000_000_000), "token_type": "Bearer" });
    apply(&mut credential, &granted).unwrap();
    assert_eq!(
        credential["tokens"]["refresh_token"], "ORIGINAL-REFRESH-TOKEN",
        "an absent refresh token means keep the one we have"
    );
}

#[test]
fn an_empty_refresh_token_is_treated_as_absent_rather_than_as_a_value() {
    let mut credential = stored();
    let granted = json!({ "access_token": token(2_000_000_000), "refresh_token": "" });
    apply(&mut credential, &granted).unwrap();
    assert_eq!(
        credential["tokens"]["refresh_token"],
        "ORIGINAL-REFRESH-TOKEN"
    );
}

#[test]
fn a_rotated_refresh_token_replaces_the_old_one() {
    // The other half of the same rule: the provider rotates on every use, so a
    // reply that carries one must be written or the next refresh fails.
    let mut credential = stored();
    let granted = json!({
        "access_token": token(2_000_000_000),
        "refresh_token": "ROTATED-REFRESH-TOKEN",
    });
    apply(&mut credential, &granted).unwrap();
    assert_eq!(
        credential["tokens"]["refresh_token"],
        "ROTATED-REFRESH-TOKEN"
    );
}

#[test]
fn a_reply_with_no_access_token_changes_nothing_and_is_reported() {
    let mut credential = stored();
    let before = credential.clone();
    let granted = json!({ "refresh_token": "ROTATED" });
    assert!(apply(&mut credential, &granted).is_err());
    assert_eq!(
        credential, before,
        "a refusal must not half-write the credential"
    );
}

#[test]
fn the_new_expiry_is_read_back_from_the_token_granted() {
    let mut credential = stored();
    let granted = json!({ "access_token": token(2_000_000_000) });
    assert_eq!(apply(&mut credential, &granted).unwrap(), 2_000_000_000);
    assert_eq!(credential["tokens"]["access_token"], token(2_000_000_000));
}

#[test]
fn the_id_token_is_only_replaced_when_one_was_granted() {
    let mut credential = stored();
    apply(
        &mut credential,
        &json!({ "access_token": token(2_000_000_000) }),
    )
    .unwrap();
    assert_eq!(credential["tokens"]["id_token"], "ORIGINAL-ID-TOKEN");

    apply(
        &mut credential,
        &json!({ "access_token": token(2_000_000_000), "id_token": "NEW-ID-TOKEN" }),
    )
    .unwrap();
    assert_eq!(credential["tokens"]["id_token"], "NEW-ID-TOKEN");
}

#[test]
fn the_account_id_is_never_touched_by_a_refresh() {
    let mut credential = stored();
    apply(
        &mut credential,
        &json!({ "access_token": token(2_000_000_000), "account_id": "SOMEONE-ELSE" }),
    )
    .unwrap();
    assert_eq!(
        credential["tokens"]["account_id"], "account-id-1",
        "the account a credential belongs to is not the refresh endpoint's to change"
    );
}

#[test]
fn the_last_refresh_stamp_is_updated_so_an_operator_can_see_it_happened() {
    let mut credential = stored();
    apply(
        &mut credential,
        &json!({ "access_token": token(2_000_000_000) }),
    )
    .unwrap();
    assert_ne!(credential["last_refresh"], "2026-09-01T00:00:00Z");
}

#[test]
fn an_expiry_is_read_without_the_signature_being_trusted() {
    // Deliberate: this decides *when to renew*, not whether to trust anything.
    // The provider stays the authority on validity.
    assert_eq!(expires_at(&token(1_790_000_000)), Some(1_790_000_000));
    assert_eq!(expires_at("not-a-jwt"), None);
    assert_eq!(expires_at(""), None);
    assert_eq!(expires_at("a.!!!!.c"), None);
}

#[test]
fn a_token_is_renewed_before_it_expires_rather_than_after() {
    let exp = 1_790_000_000;
    let token = token(exp);
    assert!(!due(&token, exp - 600), "ten minutes out, still good");
    assert!(due(&token, exp - 60), "inside the skew, renew now");
    assert!(due(&token, exp + 1), "already expired");
}

#[test]
fn a_token_whose_expiry_cannot_be_read_is_renewed_rather_than_trusted() {
    // Guessing "still valid" fails somebody's request; guessing "renew" costs one
    // call. The cheap mistake is the one to make.
    assert!(due("opaque-token", 1_790_000_000));
}
