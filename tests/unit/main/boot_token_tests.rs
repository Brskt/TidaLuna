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

/// `Some(false)` restores without a refresh, `Some(true)` restores with one, `None` abandons.
#[test]
fn a_boot_blob_is_restored_or_abandoned_by_the_generation_it_holds() {
    let (cur, new, old) = (generation("cur"), generation("new"), generation("old"));
    let mut corrupt = blob_with("at", "rt");
    corrupt.wrapped_key[0] ^= 0xFF;
    let rows = [
        (
            "current opaque",
            blob_with(&cur.opaque_at, &cur.opaque_rt),
            cur.clone(),
            None,
            Some(false),
        ),
        (
            "current real",
            blob_with(&cur.access_token, &cur.refresh_token),
            cur.clone(),
            None,
            Some(true),
        ),
        (
            "previous opaque",
            blob_with(&old.opaque_at, &old.opaque_rt),
            new.clone(),
            Some(old.clone()),
            Some(true),
        ),
        (
            "previous real",
            blob_with(&old.access_token, &old.refresh_token),
            new.clone(),
            Some(old.clone()),
            Some(true),
        ),
        (
            "an unknown blob",
            blob_with("stranger_at", "stranger_rt"),
            cur.clone(),
            Some(old.clone()),
            None,
        ),
        ("a corrupt blob", corrupt, cur.clone(), None, None),
    ];
    for (what, raw, current, previous, expected) in rows {
        let restored = match reconcile_sdk_blob(raw, stored(current, previous)) {
            BootTokenOutcome::Restore { needs_refresh, .. } => Some(needs_refresh),
            BootTokenOutcome::Abandon => None,
        };
        assert_eq!(restored, expected, "{what}");
    }
}
