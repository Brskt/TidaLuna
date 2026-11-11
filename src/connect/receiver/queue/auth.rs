use std::sync::Arc;
use std::time::{Duration, Instant};

use super::{QueueError, QueueManager, http};
use crate::connect::token_state::{
    AuthStore, CASError, GenerationStatus, RefreshGuard, TerminationReason, TokenState,
};
use crate::connect::types::ServerInfo;

impl QueueManager {
    /// Extract access/refresh tokens from the current ServerInfos and seed or
    /// advance the AuthStore. Called whenever the mobile client pushes
    /// `loadCloudQueue` or `updateServerInfo`.
    ///
    /// The wire format keeps tokens in both `auth_info.oauth_parameters` and
    /// optionally in `oauth_server_info.auth_info.oauth_parameters`. Prefer
    /// the outer copy; fall back to the inner one.
    pub(super) fn sync_auth_from_server_info(&mut self) {
        let params = http::extract_oauth_params(self.content_server.as_ref())
            .or_else(|| http::extract_oauth_params(self.queue_server.as_ref()));
        let Some((access_token, refresh_token, scope)) = params else {
            return;
        };

        // A fresh generation (login / relogin) re-arms the one-shot terminal
        // notification; the next `invalid_grant` produces exactly one
        // outgoing notification instead of being silently suppressed.
        self.terminal_notified = false;

        match self.auth.as_ref() {
            None => {
                let initial = TokenState {
                    generation: 1,
                    token_version: 1,
                    access_token,
                    refresh_token: Some(refresh_token),
                    scope,
                    expires_at: Instant::now() + Duration::from_secs(3600),
                    status: GenerationStatus::Active,
                };
                self.auth = Some(Arc::new(AuthStore::new(initial)));
            }
            Some(store) => {
                // The wire pushed a new set of tokens (relogin or quality
                // change). Bump the generation: any in-flight refresh
                // guard tied to the previous snapshot will fail its CAS.
                // Uses `store` (not CAS) because the wire is authoritative:
                // a previously-terminated generation must not block the new
                // tokens from being installed.
                let prev = store.load();
                let mut next = (*prev).clone();
                next.generation = prev.generation + 1;
                next.token_version = 1;
                next.access_token = access_token;
                next.refresh_token = Some(refresh_token);
                next.scope = scope;
                next.expires_at = Instant::now() + Duration::from_secs(3600);
                next.status = GenerationStatus::Active;
                store.store(next);
            }
        }
    }

    /// Refresh OAuth token. The HTTP exchange lives in `http::refresh_token`;
    /// this method is the façade's CAS-plus-sync layer: it takes the current
    /// `AuthStore` snapshot, calls the stateless refresh helper, then
    /// applies the outcome through `RefreshGuard`, which keeps a concurrent
    /// refresh from producing torn state.
    pub(super) async fn refresh_token(&mut self) -> Result<(), QueueError> {
        // Static OAuth server configuration (URL, grant_type, header) lives
        // in ServerInfo. The live refresh_token lives in AuthStore.
        let oauth = self
            .content_server
            .as_ref()
            .or(self.queue_server.as_ref())
            .and_then(|s| s.auth_info.as_ref())
            .and_then(|a| a.oauth_server_info.as_ref())
            .cloned()
            .ok_or(QueueError::NoOAuthServer)?;

        let store = self
            .auth
            .as_ref()
            .cloned()
            .ok_or(QueueError::NoOAuthServer)?;
        let guard = RefreshGuard::new(&store);
        let snapshot = guard.snapshot().clone();
        let current_refresh_token = snapshot
            .refresh_token
            .clone()
            .ok_or(QueueError::MissingField("refresh_token"))?;

        let success = match http::refresh_token(&self.http, &oauth, &current_refresh_token).await {
            Ok(s) => s,
            Err(QueueError::AuthTerminated { provider_error }) => {
                self.mark_generation_terminated(&snapshot, &provider_error);
                return Err(QueueError::AuthTerminated { provider_error });
            }
            Err(e) => return Err(e),
        };

        // Build the next auth state from the snapshot: `token_version`
        // only advances once per successful refresh.
        let mut next = (*snapshot).clone();
        next.token_version += 1;
        next.access_token = success.access_token.clone();
        if let Some(rt) = success.refresh_token {
            next.refresh_token = Some(rt);
        }
        next.expires_at = Instant::now() + Duration::from_secs(3600);
        next.status = GenerationStatus::Active;

        // A lost CAS is a benign race, not an error: a concurrent writer already
        // advanced the store: adopt its current token instead of replaying
        // ours (rejected by the CAS, possibly orphaned under refresh-token
        // rotation).
        let cas_result = guard.try_apply(next);
        let cas_won = cas_result.is_ok();
        let current = store.load();
        match reconcile_refresh(cas_result, &success.access_token, &current) {
            RefreshOutcome::UseToken(token) => {
                self.update_access_token(&token)?;
                if cas_won {
                    crate::vprintln!("[connect::queue] Token refreshed");
                } else {
                    crate::vprintln!(
                        "[connect::queue] Refresh superseded by a concurrent update; adopted current token"
                    );
                }
                Ok(())
            }
            RefreshOutcome::Terminal(provider_error) => {
                crate::vprintln!(
                    "[connect::queue] Refresh blocked: generation terminated ({provider_error})"
                );
                Err(QueueError::AuthTerminated { provider_error })
            }
        }
    }

    /// Mark the snapshot's generation `Terminated(InvalidGrant)`. Uses the
    /// unconditional `store`; a later stale refresh cannot erase the terminal
    /// state. `suspect_replay` stays false: replay detection is server-side
    /// (RFC 9700 §4.14.2); the client only sets the flag heuristically.
    fn mark_generation_terminated(&mut self, snapshot: &Arc<TokenState>, provider_error: &str) {
        let Some(store) = self.auth.as_ref() else {
            return;
        };
        let mut terminated = (**snapshot).clone();
        terminated.status = GenerationStatus::Terminated(TerminationReason::InvalidGrant {
            provider_error: provider_error.to_string(),
            suspect_replay: false,
        });
        store.store(terminated);
    }

    /// Mark the current generation `Terminated(Revoked)` (no pre-captured
    /// snapshot). Used when a freshly-minted token is still rejected with 401
    /// (`invalid_token`, RFC 6750 §3.1), implying out-of-band revocation.
    /// `store()` bypasses the CAS, letting a wire relogin still install fresh
    /// credentials.
    pub(super) fn mark_generation_revoked(&mut self) {
        let Some(store) = self.auth.as_ref() else {
            return;
        };
        let mut terminated = (*store.load()).clone();
        terminated.status = GenerationStatus::Terminated(TerminationReason::Revoked);
        store.store(terminated);
    }

    /// Persist a new access token into both content and queue ServerInfos.
    /// The AuthStore is the authoritative source; this keeps the wire-shaped
    /// ServerInfo in sync: outgoing HTTP Authorization headers reflect the
    /// refreshed token.
    fn update_access_token(&mut self, new_token: &str) -> Result<(), QueueError> {
        fn update_server(server: &mut Option<ServerInfo>, token: &str) {
            if let Some(s) = server
                && let Some(ref mut ai) = s.auth_info
            {
                // Update header_auth for resolve_auth_header to use the new token
                if ai.header_auth.is_some() {
                    ai.header_auth = Some(format!("Bearer {}", token));
                }
                if let Some(ref mut params) = ai.oauth_parameters {
                    params.access_token = token.to_string();
                }
                if let Some(ref mut oauth) = ai.oauth_server_info {
                    if oauth.auth_info.header_auth.is_some() {
                        oauth.auth_info.header_auth = Some(format!("Bearer {}", token));
                    }
                    if let Some(ref mut params) = oauth.auth_info.oauth_parameters {
                        params.access_token = token.to_string();
                    }
                }
            }
        }
        update_server(&mut self.content_server, new_token);
        update_server(&mut self.queue_server, new_token);
        Ok(())
    }
}

/// Outcome of a refresh attempt after reconciling the CAS result with the
/// store's current state.
#[derive(Debug, PartialEq, Eq)]
enum RefreshOutcome {
    /// Install this access token into the wire `ServerInfo` (and let the
    /// pending retry use it). Either our refresh won the CAS, or a concurrent
    /// writer installed an equally-current `Active` token that we adopt.
    UseToken(String),
    /// The generation is terminated; the caller must relogin. Carries the
    /// upstream provider error for the one-shot terminal notification.
    Terminal(String),
}

/// Reconcile a refresh CAS result with the store's current state. Pure, which makes
/// the audit-critical branch unit-testable.
///
/// A lost CAS is not a failure: adopt the winner's current token rather than
/// replay ours (rejected by the CAS, possibly orphaned under refresh-token
/// rotation). A terminated store routes to relogin.
fn reconcile_refresh(
    cas_result: Result<(), CASError>,
    our_token: &str,
    current: &TokenState,
) -> RefreshOutcome {
    match cas_result {
        Ok(()) => RefreshOutcome::UseToken(our_token.to_string()),
        Err(_) => match &current.status {
            GenerationStatus::Active => RefreshOutcome::UseToken(current.access_token.clone()),
            GenerationStatus::Terminated(reason) => {
                RefreshOutcome::Terminal(termination_provider_error(reason))
            }
        },
    }
}

/// Upstream provider error carried by a terminal generation, for the one-shot
/// relogin notification.
fn termination_provider_error(reason: &TerminationReason) -> String {
    match reason {
        TerminationReason::InvalidGrant { provider_error, .. } => provider_error.clone(),
        TerminationReason::Revoked => "revoked".to_string(),
    }
}

#[cfg(test)]
#[path = "../../../../tests/unit/connect/receiver/queue/auth.rs"]
mod tests;
