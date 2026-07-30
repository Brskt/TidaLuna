//! Tests for `src/main.rs`, attached to it by `#[path]`.

use super::*;
use platform::sdk_storage::{self, RawSdkBlob};
use platform::secure_store::{StoredTokenState, TokenGeneration};

fn generation(tag: &str) -> TokenGeneration {
    TokenGeneration {
        access_token: format!("real_at_{tag}"),
        refresh_token: format!("real_rt_{tag}"),
        opaque_at: format!("luna_at_{tag}"),
        opaque_rt: format!("luna_rt_{tag}"),
        version: 1,
        access_expires: u64::MAX,
        user_id: Some("42".into()),
        granted_scopes: vec!["r_usr".into()],
        client_id: "cid".into(),
    }
}

fn stored(current: TokenGeneration, previous: Option<TokenGeneration>) -> StoredTokenState {
    StoredTokenState {
        current,
        previous,
        previous_valid_until: None,
    }
}

/// Rebuild the RawSdkBlob a LevelDB read would produce from seed entries.
fn raw_from_entries(entries: sdk_storage::SdkEntries) -> Box<RawSdkBlob> {
    let mut salt = None;
    let mut counter = None;
    let mut wrapped_key = None;
    let mut data = None;
    for (key, value) in entries {
        match key {
            "AuthDB/tidalSalt" => salt = Some(value),
            "AuthDB/tidalCounter" => counter = Some(value),
            "AuthDB/tidalKey" => wrapped_key = Some(value),
            "AuthDB/tidalData" => data = Some(value),
            other => panic!("unexpected entry {other}"),
        }
    }
    Box::new(RawSdkBlob {
        salt: salt.unwrap().try_into().unwrap(),
        counter: counter.unwrap().try_into().unwrap(),
        wrapped_key: wrapped_key.unwrap(),
        data: data.unwrap(),
    })
}

/// A blob holding `at`/`rt`, built through the same path the seed uses.
fn blob_with(at: &str, rt: &str) -> Box<RawSdkBlob> {
    let entries =
        sdk_storage::build_seed_entries(at, rt, u64::MAX, Some("42"), &["r_usr".to_string()])
            .expect("seed entries");
    raw_from_entries(entries)
}

fn blob_tokens(raw: &RawSdkBlob) -> (String, String) {
    let credentials = sdk_storage::decrypt_raw_blob(raw).expect("blob decrypts");
    let at = credentials
        .access_token
        .and_then(|a| a.token)
        .unwrap_or_default();
    let rt = credentials.refresh_token.unwrap_or_default();
    (at, rt)
}

#[test]
fn seed_entries_roundtrip_through_decrypt() {
    let raw = blob_with("at_value", "rt_value");
    let (at, rt) = blob_tokens(&raw);
    assert_eq!(at, "at_value");
    assert_eq!(rt, "rt_value");
}

#[test]
fn current_opaque_match_restores_without_refresh() {
    let cur = generation("cur");
    let raw = blob_with(&cur.opaque_at, &cur.opaque_rt);
    let BootTokenOutcome::Restore { needs_refresh, .. } =
        reconcile_sdk_blob(raw, stored(cur, None))
    else {
        panic!("expected Restore");
    };
    assert!(!needs_refresh);
}

#[test]
fn current_real_match_needs_refresh() {
    let cur = generation("cur");
    let raw = blob_with(&cur.access_token, &cur.refresh_token);
    let BootTokenOutcome::Restore { needs_refresh, .. } =
        reconcile_sdk_blob(raw, stored(cur, None))
    else {
        panic!("expected Restore");
    };
    assert!(needs_refresh);
}

#[test]
fn previous_opaque_match_restores_with_refresh() {
    let prev = generation("old");
    let cur = generation("new");
    let raw = blob_with(&prev.opaque_at, &prev.opaque_rt);
    let BootTokenOutcome::Restore { needs_refresh, .. } =
        reconcile_sdk_blob(raw, stored(cur, Some(prev)))
    else {
        panic!("expected Restore");
    };
    assert!(needs_refresh);
}

#[test]
fn previous_real_match_restores_with_refresh() {
    let prev = generation("old");
    let cur = generation("new");
    let raw = blob_with(&prev.access_token, &prev.refresh_token);
    let BootTokenOutcome::Restore { needs_refresh, .. } =
        reconcile_sdk_blob(raw, stored(cur, Some(prev)))
    else {
        panic!("expected Restore");
    };
    assert!(needs_refresh);
}

#[test]
fn unknown_blob_abandons() {
    let raw = blob_with("stranger_at", "stranger_rt");
    let outcome = reconcile_sdk_blob(raw, stored(generation("cur"), Some(generation("old"))));
    assert!(matches!(outcome, BootTokenOutcome::Abandon));
}

#[test]
fn corrupt_blob_abandons() {
    let mut raw = blob_with("at", "rt");
    raw.wrapped_key[0] ^= 0xFF;
    let outcome = reconcile_sdk_blob(raw, stored(generation("cur"), None));
    assert!(matches!(outcome, BootTokenOutcome::Abandon));
}
