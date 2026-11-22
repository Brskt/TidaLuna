//! Rust's refresh at boot: proves the restored session's refresh token alive before anything
//! leans on it, and moves the store to a new generation when the page's blob held real tokens.
//!
//! Called from the web.loaded IPC handler while a `RefreshDuty` is owed.

use cef::{ImplBrowser, ImplFrame};

use crate::app_state::RefreshDuty;

pub(crate) fn trigger_if_needed() {
    let duty = crate::app_state::with_state(|state| std::mem::take(&mut state.refresh_duty))
        .unwrap_or_default();
    if duty == RefreshDuty::Spent {
        return;
    }

    // The epoch is read here, with the credential it refreshes, and carried through the
    // network round trip: it names the session this work answers for.
    let Some(sent) = crate::app_state::with_state(|state| {
        let ts = state.token_state.as_ref()?;
        Some(SentRefresh {
            refresh_token: ts.current.refresh_token.clone(),
            client_id: ts.current.client.id().to_owned(),
            scope: ts.current.granted_scopes.join(" "),
            session_epoch: state.session_epoch,
            version: ts.current.version,
        })
    })
    .flatten() else {
        return;
    };

    if sent.refresh_token.is_empty() {
        return;
    }

    crate::vprintln!("[AUTH]   Proactive refresh: starting ({duty:?})");
    crate::state::rt_handle().spawn(do_refresh(sent, duty));
}

/// What the token endpoint said of a refresh token Rust sent. Read on the status and `error`
/// alone, never on `error_description`. Only a request Rust sent reaches here: the page's own
/// answers never decide that a session is dead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RefreshVerdict {
    /// A 200 carrying an access token.
    Fresh,
    /// `400 invalid_grant`: the refresh token is dead.
    Dead,
    /// `401 invalid_client`: the token lives but not with that client. Nothing is touched: the
    /// stored client stays unproven and the page's next refresh sends its own.
    ClientRefused,
    /// Anything else: network, timeout, 5xx, another 4xx, a body that is not JSON.
    Inconclusive,
}

impl RefreshVerdict {
    pub(crate) fn classify(status: u16, body: &str) -> Self {
        let json = serde_json::from_str::<serde_json::Value>(body).ok();
        let field = |key: &str| {
            json.as_ref()
                .and_then(|j| j.get(key)?.as_str())
                .unwrap_or("")
        };
        match (status, field("error")) {
            (200, _) if !field("access_token").is_empty() => Self::Fresh,
            (400, "invalid_grant") => Self::Dead,
            (401, "invalid_client") => Self::ClientRefused,
            _ => Self::Inconclusive,
        }
    }
}

/// The refresh Rust sends, and the generation it was read from.
struct SentRefresh {
    refresh_token: String,
    client_id: String,
    scope: String,
    session_epoch: u64,
    version: u64,
}

/// Bounds a token endpoint that accepts the connection and never answers.
const REFRESH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

async fn do_refresh(sent: SentRefresh, duty: RefreshDuty) {
    if sent.client_id.is_empty() {
        // The token endpoint rejects a refresh grant with no client_id (400,
        // sub_status 1002): a request here can only fail. Skip it; the token
        // stays live and the SDK refreshes it near expiry.
        crate::vprintln!("[AUTH]   Proactive refresh: no client_id - skipped");
        return;
    }

    let client = &*crate::state::AUTH_CLIENT;

    let mut body = format!(
        "grant_type=refresh_token&refresh_token={}&client_id={}",
        url::form_urlencoded::byte_serialize(sent.refresh_token.as_bytes()).collect::<String>(),
        url::form_urlencoded::byte_serialize(sent.client_id.as_bytes()).collect::<String>(),
    );
    if !sent.scope.is_empty() {
        body.push_str("&scope=");
        body.push_str(
            &url::form_urlencoded::byte_serialize(sent.scope.as_bytes()).collect::<String>(),
        );
    }

    let url = format!(
        "https://{}{}",
        crate::ui::nav::HOST_AUTH,
        crate::ui::nav::PATH_OAUTH_TOKEN
    );

    let resp = match client
        .post(&url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .timeout(REFRESH_TIMEOUT)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            crate::verr!(
                "[AUTH]   Proactive refresh failed: {}",
                crate::util::network_error_text(e)
            );
            return;
        }
    };

    let status = resp.status().as_u16();
    let resp_body = resp.text().await.unwrap_or_default();

    match RefreshVerdict::classify(status, &resp_body) {
        RefreshVerdict::Fresh => {}
        RefreshVerdict::Dead => {
            end_dead_session(&sent);
            return;
        }
        RefreshVerdict::ClientRefused => {
            // The line counts how often a store holds a stale client.
            crate::verr!("[AUTH] Proactive refresh: the stored client was refused for its token");
            return;
        }
        RefreshVerdict::Inconclusive => {
            crate::vprintln!(
                "[AUTH]   Proactive refresh: {} {}",
                status,
                crate::util::truncate_str(&resp_body, 200)
            );
            return;
        }
    }

    let Ok(json) = serde_json::from_str::<serde_json::Value>(&resp_body) else {
        return;
    };
    let field = |key: &str| json.get(key).and_then(|v| v.as_str()).map(str::to_owned);
    let Some(at) = field("access_token") else {
        return;
    };
    let new_rt = field("refresh_token");
    let expires_in = json.get("expires_in").and_then(|v| v.as_u64());
    let user_id = json
        .get("user_id")
        .and_then(|v| v.as_u64())
        .map(|v| v.to_string());
    let granted_scopes: Vec<String> = field("scope")
        .map(|s| s.split(' ').map(str::to_owned).collect())
        .unwrap_or_default();

    // An update in place keeps the nonces the page holds: none is minted for it. A rotation mints
    // them off the AppState lock, and skips this cycle on RNG failure.
    let (opaque_at, opaque_rt_new) = match duty {
        RefreshDuty::CheckAndRotate => {
            let Some(opaque_at) = crate::ui::token_filter::generate_opaque() else {
                return;
            };
            let opaque_rt = match new_rt {
                Some(_) => match crate::ui::token_filter::generate_opaque() {
                    Some(o) => Some(o),
                    None => return,
                },
                None => None,
            };
            (opaque_at, opaque_rt)
        }
        RefreshDuty::Check | RefreshDuty::Spent => (String::new(), None),
    };

    let grant = crate::app_state::TokenGrant {
        access_token: &at,
        refresh_token: new_rt.as_deref(),
        opaque_at: &opaque_at,
        opaque_rt: opaque_rt_new.as_deref(),
        expires_in,
        user_id: user_id.as_deref(),
        granted_scopes: &granted_scopes,
        client_id: Some(&sent.client_id),
    };
    // The check lives INSIDE the closure, atomic with the writes it guards; `committed` carries its
    // answer out.
    let committed = crate::app_state::with_state(|state| match duty {
        RefreshDuty::Check => {
            let applied = state.refresh_in_place(
                sent.session_epoch,
                sent.version,
                &sent.refresh_token,
                &grant,
            );
            if !applied {
                crate::vprintln!(
                    "[AUTH]   Refresh answered for a generation already replaced, dropped"
                );
            }
            applied
        }
        RefreshDuty::CheckAndRotate => match state.commit_token_generation(
            sent.session_epoch,
            &grant,
            crate::app_state::UserCommit::Refresh,
        ) {
            crate::app_state::TokenCommit::Refused => {
                crate::vprintln!("[AUTH]   Refresh answered for a session already left, dropped");
                false
            }
            crate::app_state::TokenCommit::SwappedAccount => {
                crate::verr!("[AUTH] Proactive refresh answered for another account, dropped");
                false
            }
            crate::app_state::TokenCommit::Committed { durable, .. } => {
                if !durable {
                    crate::vprintln!(
                        "[AUTH]   Refresh landed with no refresh token - stored credential kept"
                    );
                }
                true
            }
        },
        RefreshDuty::Spent => false,
    })
    .unwrap_or(false);

    if !committed {
        return;
    }
    // Outside the state lock, which `eval_js` takes again. A warm boot that only refreshes has no
    // other early trigger for the scrub.
    crate::ipc::plugin::scrub_pkce_verifier();

    crate::vprintln!("[AUTH]   Proactive refresh complete ({} chars)", at.len());
}

/// The stored refresh token is dead: the session ends and the store is erased, then the page
/// reloads onto the sign-in screen with its credential blob purged before the SDK starts. Armed
/// by the boot refresh alone (at most once per process).
fn end_dead_session(sent: &SentRefresh) {
    let ended = crate::app_state::with_state(|state| {
        let ended = state.end_dead_session(sent.session_epoch, &sent.refresh_token)?;
        crate::platform::secure_store::delete_queued(&crate::state::cache_data_dir());
        Some(ended)
    })
    .flatten();
    let Some(ended) = ended else {
        crate::vprintln!("[AUTH]   Dead refresh token answered for a session already left");
        return;
    };
    crate::verr!("[AUTH] Proactive refresh: the stored refresh token is dead, session ended");
    crate::ipc::plugin::settle_ended_session(&ended);
    crate::ui::NEEDS_BOOT_BLOB_PURGE.store(true, std::sync::atomic::Ordering::Release);
    reload_desktop_page();
}

/// Reloads the main frame when it is TIDAL's desktop page; any other page takes the purge on the
/// next desktop page load.
fn reload_desktop_page() {
    let Some(Some(browser)) = crate::app_state::with_state(|state| state.browser.clone()) else {
        return;
    };
    let on_desktop = browser.main_frame().is_some_and(|frame| {
        let url = crate::ui::token_filter::userfree_to_string(&frame.url());
        crate::ui::nav::PageKind::classify(&url) == crate::ui::nav::PageKind::DesktopApp
    });
    if on_desktop {
        browser.reload();
    }
}

#[cfg(test)]
#[path = "../../tests/unit/ui/proactive_refresh.rs"]
mod tests;
