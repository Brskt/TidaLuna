//! Tests for `src/connect/receiver/queue/auth.rs`, attached to it by `#[path]`.

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

/// A lost CAS is not an error and does not replay our discarded token: the winner's current
/// token is adopted, and a terminated store routes to relogin, unless a wire relogin landed a
/// fresh Active generation while we were refreshing.
#[test]
fn a_refresh_is_reconciled_with_the_store_the_cas_left_behind() {
    for (row, cas_result, current, expected) in [
        (
            "won the CAS",
            Ok(()),
            tok("at-new", GenerationStatus::Active),
            RefreshOutcome::UseToken("at-ours".to_string()),
        ),
        (
            "lost to an Active winner",
            Err(CASError::VersionMismatch),
            tok("at-winner", GenerationStatus::Active),
            RefreshOutcome::UseToken("at-winner".to_string()),
        ),
        (
            "lost to a winner that terminated the generation",
            Err(CASError::VersionMismatch),
            tok(
                "at-x",
                GenerationStatus::Terminated(TerminationReason::InvalidGrant {
                    provider_error: "invalid_grant".to_string(),
                    suspect_replay: false,
                }),
            ),
            RefreshOutcome::Terminal("invalid_grant".to_string()),
        ),
        (
            "snapshot terminated, store still terminated",
            Err(CASError::Terminated),
            tok(
                "at-x",
                GenerationStatus::Terminated(TerminationReason::Revoked),
            ),
            RefreshOutcome::Terminal("revoked".to_string()),
        ),
        (
            "snapshot terminated, a relogin landed",
            Err(CASError::Terminated),
            tok("at-fresh", GenerationStatus::Active),
            RefreshOutcome::UseToken("at-fresh".to_string()),
        ),
    ] {
        assert_eq!(
            reconcile_refresh(cas_result, "at-ours", &current),
            expected,
            "{row}"
        );
    }
}
