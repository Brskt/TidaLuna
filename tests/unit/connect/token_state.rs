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
fn load_returns_current_snapshot() {
    let store = AuthStore::new(seed(GenerationStatus::Active));
    let snap = store.load();
    assert_eq!(snap.access_token, "at-1");
    assert_eq!(snap.token_version, 1);
}

#[test]
fn cas_succeeds_when_snapshot_is_current() {
    let store = AuthStore::new(seed(GenerationStatus::Active));
    let guard = RefreshGuard::new(&store);
    let mut new = (**guard.snapshot()).clone();
    new.token_version = 2;
    new.access_token = "at-2".to_string();
    guard.try_apply(new).expect("CAS should succeed");
    assert_eq!(store.load().token_version, 2);
    assert_eq!(store.load().access_token, "at-2");
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

#[test]
fn guard_against_stale_generation_rejects_refresh() {
    // Wire pushes a new generation while a refresh is prepared against
    // the old one: the old refresh must fail to apply (VersionMismatch),
    // so stale credentials from an earlier sign-in cannot overwrite the
    // freshly-installed ones.
    let store = AuthStore::new(seed(GenerationStatus::Active));
    let stale_guard = RefreshGuard::new(&store);

    // Simulate a wire-initiated relogin between guard creation and apply.
    let mut new_gen = (*store.load()).clone();
    new_gen.generation = 2;
    new_gen.token_version = 1;
    new_gen.access_token = "at-gen2".to_string();
    store.store(new_gen);

    let mut stale_apply = (**stale_guard.snapshot()).clone();
    stale_apply.token_version = 2; // pretend an old refresh succeeded
    stale_apply.access_token = "at-stale".to_string();
    let err = stale_guard.try_apply(stale_apply).unwrap_err();
    assert_eq!(err, CASError::VersionMismatch);

    // The wire-installed generation is preserved.
    let loaded = store.load();
    assert_eq!(loaded.generation, 2);
    assert_eq!(loaded.access_token, "at-gen2");
}

#[test]
fn generation_store_is_monotonically_replaceable() {
    // Each wire-initiated relogin bumps generation. `store` does not
    // enforce ordering, but the caller is expected to increment.
    // This test documents the contract: successive relogins observed
    // by `AuthStore::load` reflect the last `store` write, regardless
    // of the previous status.
    let store = AuthStore::new(seed(GenerationStatus::Terminated(
        TerminationReason::Revoked,
    )));
    for gen_id in 2..=4 {
        let mut next = (*store.load()).clone();
        next.generation = gen_id;
        next.token_version = 1;
        next.access_token = format!("at-gen{gen_id}");
        next.status = GenerationStatus::Active;
        store.store(next);
        let loaded = store.load();
        assert_eq!(loaded.generation, gen_id);
        assert_eq!(loaded.access_token, format!("at-gen{gen_id}"));
        assert!(matches!(loaded.status, GenerationStatus::Active));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_refreshes_only_one_wins() {
    // Two tasks prepare a refresh from the same snapshot and race to
    // apply. Exactly one CAS must succeed; the loser observes
    // VersionMismatch and must not mutate the store.
    //
    // A `Barrier` forces both tasks to finish reading their snapshots
    // before either is allowed to CAS, so the test exercises the race
    // rather than letting the scheduler serialise the two operations
    // and letting both win.
    use tokio::sync::Barrier;

    let store = Arc::new(AuthStore::new(seed(GenerationStatus::Active)));
    let barrier = Arc::new(Barrier::new(2));

    let store_a = store.clone();
    let barrier_a = barrier.clone();
    let task_a = tokio::spawn(async move {
        let guard = RefreshGuard::new(&store_a);
        let mut next = (**guard.snapshot()).clone();
        next.token_version = 2;
        next.access_token = "at-A".to_string();
        barrier_a.wait().await;
        guard.try_apply(next)
    });

    let store_b = store.clone();
    let barrier_b = barrier.clone();
    let task_b = tokio::spawn(async move {
        let guard = RefreshGuard::new(&store_b);
        let mut next = (**guard.snapshot()).clone();
        next.token_version = 2;
        next.access_token = "at-B".to_string();
        barrier_b.wait().await;
        guard.try_apply(next)
    });

    let (a, b) = tokio::join!(task_a, task_b);
    let a = a.unwrap();
    let b = b.unwrap();

    let winners = [a.is_ok(), b.is_ok()].iter().filter(|x| **x).count();
    assert_eq!(winners, 1, "exactly one CAS must win");

    let loaded = store.load();
    assert_eq!(loaded.token_version, 2);
    assert!(loaded.access_token == "at-A" || loaded.access_token == "at-B");
}
