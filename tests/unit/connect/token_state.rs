//! Tests for `src/connect/token_state.rs`, attached to it by `#[path]`.

use super::*;
use std::time::Duration;

fn seed(status: GenerationStatus) -> TokenState {
    TokenState {
        generation: 1,
        token_version: 1,
        access_token: "at-1".to_string(),
        refresh_token: Some("rt-1".to_string()),
        scope: Some("read".to_string()),
        expires_at: Instant::now() + Duration::from_secs(3600),
        status,
    }
}

#[test]
fn cas_rejects_stale_snapshot() {
    let store = AuthStore::new(seed(GenerationStatus::Active));
    let guard_a = RefreshGuard::new(&store);
    let guard_b = RefreshGuard::new(&store);

    // Guard A wins the race.
    let mut new_a = (**guard_a.snapshot()).clone();
    new_a.token_version = 2;
    guard_a.try_apply(new_a).expect("A should win");

    // Guard B tries to apply from the stale snapshot.
    let mut new_b = (**guard_b.snapshot()).clone();
    new_b.token_version = 2;
    new_b.access_token = "at-b".to_string();
    let err = guard_b.try_apply(new_b).unwrap_err();
    assert_eq!(err, CASError::VersionMismatch);

    // A's write is preserved; B did not overwrite it.
    assert_eq!(store.load().access_token, "at-1");
    assert_eq!(store.load().token_version, 2);
}

#[test]
fn cas_refuses_updates_on_terminated_generation() {
    let store = AuthStore::new(seed(GenerationStatus::Terminated(
        TerminationReason::Revoked,
    )));
    let guard = RefreshGuard::new(&store);
    let mut new = (**guard.snapshot()).clone();
    new.token_version = 2;
    let err = guard.try_apply(new).unwrap_err();
    assert_eq!(err, CASError::Terminated);
}

#[test]
fn cas_allows_active_to_terminated_transition() {
    // Moving INTO Terminated from a non-terminal snapshot must still
    // be possible: that's how `mark_generation_terminated` records an
    // invalid_grant.
    let store = AuthStore::new(seed(GenerationStatus::Active));
    let guard = RefreshGuard::new(&store);
    let mut new = (**guard.snapshot()).clone();
    new.status = GenerationStatus::Terminated(TerminationReason::InvalidGrant {
        provider_error: "invalid_grant".to_string(),
        suspect_replay: false,
    });
    guard
        .try_apply(new)
        .expect("terminating transition allowed");
    assert!(matches!(
        store.load().status,
        GenerationStatus::Terminated(TerminationReason::InvalidGrant { .. })
    ));
}

#[test]
fn store_bypasses_terminated_guard() {
    // Relogin from the wire must be able to install fresh tokens even
    // when the current generation was terminated by invalid_grant.
    let store = AuthStore::new(seed(GenerationStatus::Terminated(
        TerminationReason::InvalidGrant {
            provider_error: "invalid_grant".to_string(),
            suspect_replay: false,
        },
    )));
    let mut fresh = (*store.load()).clone();
    fresh.generation = 2;
    fresh.token_version = 1;
    fresh.access_token = "new-at".to_string();
    fresh.status = GenerationStatus::Active;
    store.store(fresh);

    let loaded = store.load();
    assert_eq!(loaded.generation, 2);
    assert_eq!(loaded.access_token, "new-at");
    assert!(matches!(loaded.status, GenerationStatus::Active));
}
