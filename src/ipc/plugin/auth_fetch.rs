use super::{ipc_callback_err, ipc_callback_ok, take_ipc_callback};
use crate::app_state::{IpcCallback, IpcMessage, with_state};

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
        if super::proxy::leaks_real_token(&url, payload) {
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

    // Reject if the OAuth token hasn't been captured yet - avoids sending unauthenticated
    // requests that would 403 and get memoized as failures on the frontend.
    let token = with_state(|state| state.captured_token.clone()).unwrap_or_default();
    if token.is_empty() {
        ipc_callback_err(&callback, 401, "tidal.fetch: auth token not yet captured");
        return;
    }

    dispatch_authenticated_fetch(
        "@luna/lib".to_string(),
        url,
        &opts_json,
        query_id,
        callback,
        TokenSource::Captured(token),
    );
}

/// Where `dispatch_authenticated_fetch` takes the token it attaches.
enum TokenSource<'a> {
    /// `tidal.fetch`, plugins included (through `TidalApi`): its handler checked only the TIDAL
    /// API host and a non-empty captured token; no session check.
    Captured(String),
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
    let opts: crate::plugins::fetch::FetchOpts =
        serde_json::from_str(opts_json).unwrap_or_else(|_| serde_json::from_str("{}").unwrap());
    let token = match source {
        TokenSource::Captured(token) => Some(token),
        // The comparison and the credential read share one lock deliberately: a session change
        // landing between them would hand out the very token the comparison exists to refuse.
        // Same shape as the guards at the token mint sites, and for the same reason.
        TokenSource::Capability(cap) => with_state(|state| {
            (state.plugin_manager.capability_epoch(cap) == Some(state.session_epoch))
                .then(|| state.captured_token.clone())
        })
        .flatten(),
    };
    let Some(token) = token else {
        // A capability minted under a session the user has left. The plugin holding it was
        // injected as that session ended, and nothing tracks it any more. This is the last
        // place that can refuse it the credential.
        crate::vprintln!("[PLUGIN:FETCH] REFUSED '{plugin_id}': capability outlived its session");
        ipc_callback_err(
            &callback,
            403,
            "plugin.fetch: capability belongs to a past session",
        );
        return;
    };
    with_state(|state| {
        state.pending_ipc_callbacks.insert(query_id, callback);
    });
    crate::state::rt_handle().spawn(async move {
        let result = crate::plugins::fetch::plugin_fetch(&plugin_id, &url, &opts, &token).await;
        let Some(cb) = take_ipc_callback(query_id) else {
            return;
        };
        match result {
            // Defence-in-depth: scrub any leaked real token before plugin JS sees it.
            Ok(json) => ipc_callback_ok(&cb, &super::proxy::scrub_real_tokens(json)),
            Err(e) => ipc_callback_err(&cb, 500, &e),
        }
    });
}
