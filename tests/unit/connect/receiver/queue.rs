//! Tests for `src/connect/receiver/queue/mod.rs`, attached to it by `#[path]`.

use super::*;

fn tok(access: &str, status: GenerationStatus) -> TokenState {
    TokenState {
        generation: 1,
        token_version: 1,
        access_token: access.to_string(),
        refresh_token: Some("rt".to_string()),
        scope: Some("read".to_string()),
        expires_at: Instant::now() + Duration::from_secs(3600),
        status,
    }
}

#[test]
fn won_cas_uses_our_token() {
    // We won the CAS: the store already holds our freshly-minted token.
    let current = tok("at-new", GenerationStatus::Active);
    let outcome = reconcile_refresh(Ok(()), "at-new", &current);
    assert_eq!(outcome, RefreshOutcome::UseToken("at-new".to_string()));
}

#[test]
fn lost_cas_with_active_winner_adopts_winner_token() {
    // The audit bug: a benign VersionMismatch must NOT surface as an error
    // and must NOT replay our discarded token. A concurrent writer already
    // installed a current Active token; adopt the WINNER's token.
    let current = tok("at-winner", GenerationStatus::Active);
    let outcome = reconcile_refresh(Err(CASError::VersionMismatch), "at-ours", &current);
    assert_eq!(outcome, RefreshOutcome::UseToken("at-winner".to_string()));
}

#[test]
fn lost_cas_with_terminated_winner_is_terminal() {
    // VersionMismatch where the winner terminated the generation: relogin.
    let current = tok(
        "at-x",
        GenerationStatus::Terminated(TerminationReason::InvalidGrant {
            provider_error: "invalid_grant".to_string(),
            suspect_replay: false,
        }),
    );
    let outcome = reconcile_refresh(Err(CASError::VersionMismatch), "at-ours", &current);
    assert_eq!(
        outcome,
        RefreshOutcome::Terminal("invalid_grant".to_string())
    );
}

#[test]
fn terminated_snapshot_is_terminal() {
    // Our snapshot's generation was already terminated at CAS time and the
    // store is still terminated: route to relogin, never InvalidResponse.
    let current = tok(
        "at-x",
        GenerationStatus::Terminated(TerminationReason::Revoked),
    );
    let outcome = reconcile_refresh(Err(CASError::Terminated), "at-ours", &current);
    assert_eq!(outcome, RefreshOutcome::Terminal("revoked".to_string()));
}

#[test]
fn terminated_snapshot_but_relogin_landed_adopts_fresh() {
    // Snapshot was terminated, but a wire relogin installed a fresh Active
    // generation while we were refreshing: adopt the fresh token.
    let current = tok("at-fresh", GenerationStatus::Active);
    let outcome = reconcile_refresh(Err(CASError::Terminated), "at-ours", &current);
    assert_eq!(outcome, RefreshOutcome::UseToken("at-fresh".to_string()));
}
