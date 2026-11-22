use super::{ipc_callback_err, ipc_callback_ok, take_ipc_callback};
use crate::app_state::{IpcCallback, IpcMessage, PluginCredential, with_state};
use crate::plugins::outbound::{OutboundRequest, PluginFetchError};

pub(super) fn handle_plugin_fetch(
    msg: &IpcMessage,
    caller: &crate::ipc::caller::Attributed,
    query_id: i64,
    callback: IpcCallback,
) {
    // Taken from the gate's own resolution rather than read off argument 0 or resolved again: the
    // plugin named in a blocked-exfiltration line is the one the gate actually admitted.
    let plugin_id = caller.plugin_id.to_string();
    let url = msg.arg(1).to_string();
    let raw_opts = msg.arg(2);
    let opts_json = if raw_opts.is_empty() { "{}" } else { raw_opts }.to_string();

    // Block non-Tidal requests that carry a real token in URL or opts (body/headers).
    if !crate::plugins::fetch::is_tidal_api(&url) {
        let payload = if opts_json == "{}" {
            None
        } else {
            Some(opts_json.as_str())
        };
        if super::scrub::leaks_real_token(&url, payload) {
            crate::vprintln!(
                "[PLUGIN:FETCH] BLOCKED token exfiltration from '{}' to {}",
                plugin_id,
                crate::util::truncate_str(&crate::util::redact_url_query(&url), 80)
            );
            ipc_callback_err(&callback, 403, "plugin.fetch: request blocked");
            return;
        }
    }

    // The capability travels on for the credential check. Attribution alone cannot make it:
    // a plugin unloading after a logout is still legitimately itself, which is why the
    // capability outlives the unload at all; what it must not still buy is a live token.
    dispatch_authenticated_fetch(
        plugin_id,
        url,
        &opts_json,
        query_id,
        callback,
        TokenSource::Capability(caller.capability),
    );
}

/// Authenticated fetch restricted to TIDAL API hosts.
/// Used by core bundle (@luna/lib) - the token never leaves Rust.
pub(super) fn handle_tidal_fetch(msg: &IpcMessage, query_id: i64, callback: IpcCallback) {
    let url = msg.arg(0).to_string();
    let raw_opts = msg.arg(1);
    let opts_json = if raw_opts.is_empty() { "{}" } else { raw_opts }.to_string();

    // Reject non-TIDAL URLs - this channel only serves authenticated TIDAL API requests
    if !crate::plugins::fetch::is_tidal_api(&url) {
        ipc_callback_err(
            &callback,
            403,
            &format!("tidal.fetch rejected: not a TIDAL API URL ({url})"),
        );
        return;
    }

    // Reject while no session holds a token: an unauthenticated request would 403 and be
    // memoized as a failure on the frontend.
    let Some(token) = with_state(|state| state.access_token().map(str::to_owned)).flatten() else {
        ipc_callback_err(&callback, 401, "tidal.fetch: no session holds a token");
        return;
    };

    dispatch_authenticated_fetch(
        "@luna/lib".to_string(),
        url,
        &opts_json,
        query_id,
        callback,
        TokenSource::Session(token),
    );
}

/// Where `dispatch_authenticated_fetch` takes the token it attaches.
enum TokenSource<'a> {
    /// `tidal.fetch`, plugins included (through `TidalApi`): its handler checked only the TIDAL
    /// API host and the session's stored token; no capability check.
    Session(String),
    /// `plugin.fetch`: honoured only under the session that minted the capability.
    Capability(&'a str),
}

fn dispatch_authenticated_fetch(
    plugin_id: String,
    url: String,
    opts_json: &str,
    query_id: i64,
    callback: IpcCallback,
    source: TokenSource,
) {
    // Before the credential and before the callback is parked: a refused request reads no token.
    let request = match OutboundRequest::from_object(&url, opts_json) {
        Ok(request) => request,
        Err(refusal) => {
            crate::vprintln!(
                "[PLUGIN:FETCH] REFUSED '{plugin_id}' to {}: {refusal}",
                crate::util::refused_host(&url)
            );
            ipc_callback_err(
                &callback,
                refusal.status() as i32,
                &format!("plugin.fetch refused: {refusal}"),
            );
            return;
        }
    };
    let credential = match source {
        TokenSource::Session(token) => PluginCredential::Credential(token),
        // The comparison and the credential read share one lock deliberately: a session change
        // landing between them would hand out the very token the comparison exists to refuse.
        // Same shape as the guards at the token mint sites, and for the same reason.
        TokenSource::Capability(cap) => with_state(|state| {
            PluginCredential::for_plugin(
                state.plugin_manager.capability_epoch(cap),
                state.session_epoch,
                state.token_state.as_ref(),
            )
        })
        .unwrap_or(PluginCredential::PastSession),
    };
    let token = match credential {
        PluginCredential::Credential(token) => token,
        // Sent without a token, as a plugin's request always was outside a session.
        PluginCredential::NoCredential => String::new(),
        PluginCredential::PastSession => {
            // A capability minted under a session the user has left. The plugin holding it was
            // injected as that session ended, and nothing tracks it any more. This is the last
            // place that can refuse it the credential.
            crate::vprintln!(
                "[PLUGIN:FETCH] REFUSED '{plugin_id}': capability outlived its session"
            );
            ipc_callback_err(
                &callback,
                403,
                "plugin.fetch: capability belongs to a past session",
            );
            return;
        }
    };
    with_state(|state| {
        state.pending_ipc_callbacks.insert(query_id, callback);
    });
    crate::state::rt_handle().spawn(async move {
        let result = crate::plugins::fetch::plugin_fetch(&plugin_id, request, &token).await;
        let Some(cb) = take_ipc_callback(query_id) else {
            return;
        };
        match result {
            // Defence-in-depth: scrub any leaked real token before plugin JS sees it.
            Ok(json) => ipc_callback_ok(&cb, &super::scrub::scrub_real_tokens(json)),
            Err(e @ PluginFetchError::Refused(_)) => {
                ipc_callback_err(&cb, 403, &format!("plugin.fetch {e}"))
            }
            Err(e) => ipc_callback_err(&cb, 500, &format!("plugin.fetch {e}")),
        }
    });
}
