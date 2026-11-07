use super::jsrt::PassGuard;
use super::{ipc_callback_err, ipc_callback_ok, take_ipc_callback};
use crate::app_state::{IpcCallback, IpcMessage, Ticket, with_state};

pub(super) fn handle_plugin_fetch(
    msg: &IpcMessage,
    caller: &crate::ipc::caller::Caller,
    query_id: i64,
    callback: IpcCallback,
) {
    // Taken from the gate's own resolution rather than read off argument 0 or resolved again: the
    // plugin named in a blocked-exfiltration line is the one the gate actually admitted.
    let plugin_id = match caller {
        crate::ipc::caller::Caller::Plugin(url) => url.clone(),
        crate::ipc::caller::Caller::Unattributed => String::new(),
    };
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
        None,
        msg.cap.as_deref(),
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
        Some(token),
        None,
    );
}

/// `capability`, when present, is checked against the current session before the token is
/// handed over. `None` means the caller is the core bundle, which holds no capability and
/// already validated its own token.
fn dispatch_authenticated_fetch(
    plugin_id: String,
    url: String,
    opts_json: &str,
    query_id: i64,
    callback: IpcCallback,
    pre_validated_token: Option<String>,
    capability: Option<&str>,
) {
    let opts: crate::plugins::fetch::FetchOpts =
        serde_json::from_str(opts_json).unwrap_or_else(|_| serde_json::from_str("{}").unwrap());
    let token = match pre_validated_token {
        Some(token) => Some(token),
        // The comparison and the credential read share one lock deliberately: a session change
        // landing between them would hand out the very token the comparison exists to refuse.
        // Same shape as the guards at the token mint sites, and for the same reason.
        None => with_state(|state| match capability {
            Some(cap)
                if state.plugin_manager.capability_epoch(cap) != Some(state.session_epoch) =>
            {
                None
            }
            _ => Some(state.captured_token.clone()),
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

pub(super) fn handle_plugin_fetch_package(msg: &IpcMessage, callback: IpcCallback) {
    let url = msg.arg(0).to_string();
    crate::state::rt_handle().spawn(async move {
        let client = &*crate::state::HTTP_CLIENT;
        match fetch_plugin_package(client, &url).await {
            Ok(Some(manifest)) => ipc_callback_ok(&callback, &manifest),
            Ok(None) => ipc_callback_err(&callback, 500, "plugin manifest not found"),
            Err(e) => ipc_callback_err(&callback, 500, &format!("{e:#}")),
        }
    });
}

pub(super) fn handle_plugin_install(msg: &IpcMessage, callback: IpcCallback) {
    let url = msg.arg(0).to_string();
    crate::state::rt_handle().spawn(async move {
        match do_plugin_install(url).await {
            Ok(info) => {
                let json = serde_json::to_string(&info).unwrap_or_else(|_| "null".to_string());
                ipc_callback_ok(&callback, &json);
            }
            Err(InstallError::Refused(refusal)) => {
                ipc_callback_err(&callback, refusal.code(), &refusal.to_string());
            }
            Err(InstallError::Failed(e)) => ipc_callback_err(&callback, 500, &e),
        }
    });
}

/// Enable: deps -> DB enable -> transpile -> eval_js -> persist `ever_dispatched`.
/// Not one transaction (DB write + renderer eval can't be); a crash mid-sequence
/// self-heals on the next load via `do_load_plugins_inline` (re-inject/auto-disable).
pub(super) fn handle_plugin_enable(msg: &IpcMessage, callback: IpcCallback) {
    let url = msg.arg(0).to_string();
    // Taken here, on the UI thread, with the page that asked: read after the awaits below, it
    // would name whatever page replaced that one meanwhile.
    let Some(ticket) = with_state(|state| state.ticket()) else {
        ipc_callback_err(&callback, 500, "State unavailable");
        return;
    };
    crate::state::rt_handle().spawn(async move {
        match do_plugin_enable(url, ticket).await {
            Ok(()) => ipc_callback_ok(&callback, "true"),
            Err(e) => ipc_callback_err(&callback, 500, &e),
        }
    });
}

async fn do_plugin_enable(url: String, ticket: Ticket) -> Result<(), String> {
    super::jsrt::in_section(move |pass| enable_in_section(pass, &url, ticket)).await?
}

/// The whole enable under the pass lock; no other writer of the row or the load lands between
/// its steps. A panic past the mark undoes the load this section marked, named by its id.
fn enable_in_section(pass: &PassGuard, url: &str, ticket: Ticket) -> Result<(), String> {
    // Before the flag is set. A refusal leaves nothing to revert.
    let admission =
        super::admission::admit(pass, ticket).map_err(|closed| closed.reason().to_string())?;
    let marked_before = with_state(|state| state.plugin_manager.last_marked(url)).flatten();
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        enable_admitted(&admission, url)
    })) {
        Ok(enabled) => enabled,
        Err(_) => {
            let marked = with_state(|state| state.plugin_manager.last_marked(url)).flatten();
            if let Some(load_id) = marked.filter(|id| Some(*id) != marked_before) {
                super::inject::undo_injection(url, load_id);
            }
            revert_enable(url);
            Err("The plugin could not be enabled".to_string())
        }
    }
}

fn enable_admitted(admission: &super::admission::Admission, url: &str) -> Result<(), String> {
    let db = crate::state::db();

    // 1. Check name + deps, set enabled=1, get code (all DB ops)
    let (plugin_name, code) = {
        let url = url.to_string();
        db.call_plugins(move |pc| {
            // The stored name, the one the startup pass loads the plugin under.
            let crate::plugins::store::StoredPlugin {
                name,
                manifest,
                code,
            } = crate::plugins::store::stored(pc, &url)
                .map_err(|e| format!("DB error: {e}"))?
                .ok_or_else(|| format!("Plugin not found: {url}"))?;
            let name = crate::plugins::name::PluginName::parse(&name)
                .map_err(|refused| format!("Cannot enable: {refused}"))?;

            // Check dependencies are satisfied
            if let Err(missing) = crate::plugins::store::check_dependencies_satisfied(pc, &manifest)
            {
                return Err(format!(
                    "Cannot enable: deps not satisfied: {}",
                    missing.join(", ")
                ));
            }

            // DB: set enabled=1
            crate::plugins::store::enable(pc, &url).map_err(|e| format!("DB error: {e}"))?;

            Ok((name, code))
        })
    }?;

    // 2. Mark loading, transpile and dispatch, through the one owner of that sequence, under the
    // admission's ticket: `inject_plugin` re-reads it after the dispatch to decide whether the
    // code may stay.
    let load_id = match super::inject::inject_plugin(admission, url, &plugin_name, &code) {
        Ok(load_id) => load_id,
        // No `InjectFailure` leaves a load of this attempt registered. An enable still owes the
        // `enabled` flag it set, undone only when the plugin itself failed: a reload, a logout or
        // this machine's failure says nothing about it; the next pass loads it.
        Err(failure) => {
            if let super::inject::PassDuty::Reconcile = failure.pass_duty() {
                revert_enable(url);
            }
            return Err(failure.reason());
        }
    };

    // 3. Persistent flag: the code was dispatched; the cleanup guard depends on it.
    //    If this fails, revert the activation: better a failed enable than an inconsistent flag.
    let url_flag = url.to_string();
    let flag =
        db.call_plugins(move |pc| crate::plugins::store::mark_ever_dispatched(pc, &url_flag));
    if flag.is_err() {
        crate::vprintln!(
            "[PLUGIN] Failed to persist ever_dispatched for '{}' - reverting activation",
            url
        );
        super::inject::undo_injection(url, load_id);
        revert_enable(url);
        return Err("Failed to persist dispatch flag".to_string());
    }

    // 4. Timeout: no ready ack after ten idle seconds, mark failed + notify frontend
    crate::state::rt_handle().spawn(super::inject::watch_ack(
        url.to_string(),
        load_id,
        admission.ticket(),
    ));

    Ok(())
}

fn revert_enable(url: &str) {
    let url = url.to_string();
    let _ =
        crate::state::db().call_plugins(move |pc| crate::plugins::store::disable_failed(pc, &url));
}

/// Atomic disable: guard dependants -> DB disable -> eval_js cleanup -> mark_unloaded.
pub(super) fn handle_plugin_disable(msg: &IpcMessage, callback: IpcCallback) {
    let url = msg.arg(0).to_string();
    crate::state::rt_handle().spawn(async move {
        match do_plugin_disable(url).await {
            Ok(()) => ipc_callback_ok(&callback, "true"),
            Err(e) => ipc_callback_err(&callback, 500, &e),
        }
    });
}

async fn do_plugin_disable(url: String) -> Result<(), String> {
    super::jsrt::in_section(move |pass| {
        // 1. Guard: check enabled dependants + DB disable
        {
            let url = url.clone();
            crate::state::db().call_plugins(move |pc| {
                let name = crate::plugins::store::get_name_by_url(pc, &url)
                    .ok_or_else(|| format!("Plugin not found: {url}"))?;

                match crate::plugins::store::find_enabled_dependants(pc, &name) {
                    Err(msg) => return Err(msg),
                    Ok(deps) if !deps.is_empty() => {
                        return Err(format!(
                            "Cannot disable '{name}': active dependants: {}",
                            deps.join(", ")
                        ));
                    }
                    _ => {}
                }

                // DB: set enabled=0
                crate::plugins::store::disable(pc, &url).map_err(|e| format!("DB error: {e}"))
            })
        }?;
        // 2. Unload from the renderer the load current now: under the pass lock no enable marks
        // another between the row and here. Nothing loaded is fine, the row was all this owed.
        super::inject::unload_current(pass, &url);
        Ok(())
    })
    .await?
}

/// jsrt.load_plugins as request-response: runs multi-pass + reconciliation, then responds.
/// Refuses to load plugins when no session is active (login page scenario).
pub(super) fn has_session() -> bool {
    with_state(|state| !state.captured_token.is_empty() || state.token_state.is_some())
        .unwrap_or(false)
}

pub(super) fn handle_jsrt_load_plugins(callback: IpcCallback) {
    if !has_session() {
        crate::vprintln!("[PLUGIN] Skipping plugin load - no active session");
        ipc_callback_ok(&callback, "true");
        return;
    }

    // Gate: on a cold boot the real OAuth token still lives in TIDAL's SDK until
    // the proactive refresh rotates it to opaque nonces. Park the reply until the
    // gate opens (refresh done or 5s safety timeout); plugins can't read it.
    // Parked first, then requested. Parking is what makes a second caller join the pass already
    // running instead of starting its own over the same plugins, and it is the only way its
    // answer can arrive: the pass, not this handler, replies.
    let gate_closed = with_state(|state| {
        let ticket = state.ticket();
        state.plugin_load_waiters.push((ticket, callback.clone()));
        !state.proactive_refresh_done
    })
    .unwrap_or(false);

    if gate_closed {
        crate::vprintln!("[PLUGIN] Deferring plugin load until proactive refresh completes");
        super::jsrt::arm_gate_timeout();
        return;
    }

    super::jsrt::request_plugin_load();
}

/// Check for code changes without modifying the DB. Returns `{ hash: "..." }` or error.
/// Used by live reload polling.
pub(super) fn handle_plugin_check_hash(msg: &IpcMessage, query_id: i64, callback: IpcCallback) {
    let url = msg.arg(0).to_string();
    with_state(|state| {
        state.pending_ipc_callbacks.insert(query_id, callback);
    });
    crate::state::rt_handle().spawn(async move {
        let client = &*crate::state::HTTP_CLIENT;
        let base = sanitize_plugin_url(&url);
        let code_url = format!("{base}.mjs");

        let result = fetch_text_capped(client, &code_url, MAX_CODE_BYTES, "plugin code").await;

        // After the fetch, not before: a cancellation mid-flight must find the entry still in
        // the map for `on_query_canceled` to drop, or this task resolves a retired callback.
        let Some(cb) = take_ipc_callback(query_id) else {
            return;
        };

        match result {
            Ok(code) => {
                let hash = fnv_hash_str(&code);
                let json = format!(r#"{{"hash":"{hash}"}}"#);
                ipc_callback_ok(&cb, &json);
            }
            Err(e) => ipc_callback_err(&cb, 500, &format!("{e:#}")),
        }
    });
}

/// What an uninstall must clear, typed rather than a bare name: an unknown url produced an empty name,
/// which every clear below reads as "all". An unmatched `plugin.uninstall` wiped every plugin's
/// trust rows, tokens and capabilities.
#[derive(Clone)]
pub(super) enum CleanupScope {
    /// One plugin, by canonical manifest name.
    Plugin(crate::plugins::name::PluginName),
    /// Everything, from `plugin.uninstall_all`.
    All,
}

/// The scope an uninstall leaves behind. No name means nothing matched, and nothing is cleared.
/// Nor does a stored name the rule refuses: startup and enable never load such a plugin (nothing
/// in memory belongs to it), and none can be installed under that name to inherit its rows.
pub(super) fn cleanup_scope_for_uninstall(name: String) -> Option<CleanupScope> {
    crate::plugins::name::PluginName::parse(&name)
        .ok()
        .map(CleanupScope::Plugin)
}

pub(super) fn handle_plugin_db(
    msg: IpcMessage,
    caller: &crate::ipc::caller::Caller,
    callback: IpcCallback,
) {
    let db = crate::state::db();
    let channel = msg.channel.clone();
    let args = msg.args.clone();
    // The storage namespace is the caller's url, never argument 0: that named a namespace with no
    // check the caller owned it. Taken from the gate's resolution rather than resolved a second
    // time, which could answer differently and silently move the write.
    let caller_ns = match caller {
        crate::ipc::caller::Caller::Plugin(url) => url.clone(),
        crate::ipc::caller::Caller::Unattributed => String::new(),
    };

    // Storage rows are keyed by the caller: an unattributed one has no namespace, and `""` is not
    // inert (it is a row every unattributed caller would share). The gate already refuses these
    // channels; this is the floor under it.
    if channel.starts_with("plugin.storage.") && caller_ns.is_empty() {
        ipc_callback_err(&callback, 403, "unattributed call");
        return;
    }

    if channel == "plugin.uninstall" || channel == "plugin.uninstall_all" {
        handle_plugin_uninstall(channel, msg.arg(0).to_string(), callback);
        return;
    }

    // Posted rather than called: this arrives on the CEF UI thread, where waiting on the actor
    // freezes rendering and input for the whole round trip. The actor answers the renderer from
    // its own thread, the callback marshalling back to the UI thread by itself.
    db.post(move |pc, _| {
        // Wrapped in a closure to let the arms below answer with `return Err(...)`: the enclosing
        // closure owes the actor nothing, and a bare return there would abandon the reply the
        // renderer waits on.
        let result: Result<String, String> = (|| match channel.as_str() {
            "plugin.list" => {
                let plugins = crate::plugins::store::list(pc);
                Ok(serde_json::to_string(&plugins).unwrap_or_else(|_| "[]".to_string()))
            }
            "plugin.storage.get" => {
                let ns = caller_ns.as_str();
                let key = args.get(1).and_then(|v| v.as_str()).unwrap_or("");
                match crate::plugins::store::storage_get(pc, ns, key) {
                    Some(val) => {
                        Ok(serde_json::to_string(&val).unwrap_or_else(|_| "null".to_string()))
                    }
                    None => Ok("null".to_string()),
                }
            }
            "plugin.storage.set" => {
                let ns = caller_ns.as_str();
                let key = args.get(1).and_then(|v| v.as_str()).unwrap_or("");
                let value = args.get(2).and_then(|v| v.as_str()).unwrap_or("");
                crate::plugins::store::storage_set(pc, ns, key, value)
                    .map(|()| "true".to_string())
                    .map_err(|e| e.to_string())
            }
            "plugin.storage.del" => {
                let ns = caller_ns.as_str();
                let key = args.get(1).and_then(|v| v.as_str()).unwrap_or("");
                crate::plugins::store::storage_del(pc, ns, key)
                    .map(|()| "true".to_string())
                    .map_err(|e| e.to_string())
            }
            "plugin.storage.keys" => {
                let ns = caller_ns.as_str();
                let keys = crate::plugins::store::storage_keys(pc, ns);
                Ok(serde_json::to_string(&keys).unwrap_or_else(|_| "[]".to_string()))
            }
            "plugin.cleanup_failed_install" => {
                let url = args.first().and_then(|v| v.as_str()).unwrap_or("");
                // Triple guard: enabled=0 AND ever_dispatched=0
                let (enabled, ever_dispatched): (bool, bool) = pc
                    .query_row(
                        "SELECT enabled, ever_dispatched FROM plugins WHERE url = ?1 AND installed = 1",
                        rusqlite::params![url],
                        |row| Ok((row.get::<_, i32>(0)? != 0, row.get::<_, i32>(1)? != 0)),
                    )
                    .unwrap_or((true, true));
                if enabled {
                    return Err("Cannot cleanup: plugin is currently enabled.".to_string());
                }
                if ever_dispatched {
                    return Err(
                        "Cannot cleanup: plugin code was previously dispatched. Use plugin.uninstall instead."
                            .to_string(),
                    );
                }
                crate::plugins::store::uninstall(pc, url)
                    .map(|()| "true".to_string())
                    .map_err(|e| e.to_string())
            }
            _ => Err(format!("Unknown plugin channel: {}", channel)),
        })();

        match result {
            Ok(json) => ipc_callback_ok(&callback, &json),
            Err(e) => ipc_callback_err(&callback, 500, &e),
        }
    });
}

/// `plugin.uninstall` (of `target`) and `plugin.uninstall_all`, as one section under the pass lock.
/// Off the UI thread it arrives on, and off the actor: a section waits on the actor.
fn handle_plugin_uninstall(channel: String, target: String, callback: IpcCallback) {
    crate::state::rt_handle().spawn(async move {
        let all = channel == "plugin.uninstall_all";
        let done = super::jsrt::in_section(move |pass| uninstall_in_section(pass, all, &target));
        match done.await.and_then(|done| done) {
            Ok(()) => ipc_callback_ok(&callback, "true"),
            Err(e) => ipc_callback_err(&callback, 500, &e),
        }
    });
}

/// The rows first, leaving the plugin as it was on a refusal or a DB error; the page part runs
/// whatever they said, a row deduplicated away having left its load. Trust and native modules go
/// once the loads are gone, as on a reinstall: a load still running would ask or register again.
fn uninstall_in_section(pass: &PassGuard, all: bool, target: &str) -> Result<(), String> {
    let url = target.to_string();
    let db = crate::state::db();
    let scope = db.call_plugins(move |pc| {
        if all {
            crate::plugins::store::uninstall_all(pc).map_err(|e| e.to_string())?;
            Ok(Some(CleanupScope::All))
        } else {
            let name = crate::plugins::store::get_name_by_url(pc, &url).unwrap_or_default();
            if !name.is_empty() {
                match crate::plugins::store::find_dependants(pc, &name) {
                    Err(msg) => return Err(msg),
                    Ok(deps) if !deps.is_empty() => {
                        return Err(format!(
                            "Cannot uninstall '{name}': required by {}",
                            deps.join(", ")
                        ));
                    }
                    _ => {}
                }
            }
            crate::plugins::store::uninstall(pc, &url).map_err(|e| e.to_string())?;
            Ok(cleanup_scope_for_uninstall(name))
        }
    })?;
    if all {
        let loads = with_state(|state| state.plugin_manager.loaded_loads()).unwrap_or_default();
        for (url, load_id) in loads {
            super::inject::undo_injection(&url, load_id);
        }
    } else {
        super::inject::unload_current(pass, target);
    }
    // Every clear matches on the scope: no name or url stands for every plugin. The uninstall is
    // done; a failed trust reset is reported, not answered as a failed uninstall.
    if let Some(scope) = scope {
        let trust_scope = scope.clone();
        if let Err(e) = db.call_settings(move |sc| super::native::forget_trust(sc, &trust_scope)) {
            crate::verr!("[PLUGIN] Could not clear the trust of an uninstalled plugin: {e}");
        }
        super::native::clear_native_channels(&scope);
    }
    // Uninstall is terminal: the capabilities go too, and a leftover renderer closure stops writing
    // as this plugin. A disable keeps them for its unload handler.
    if all {
        with_state(|state| {
            state.plugin_manager.revoke_all_capabilities();
            state.plugin_manager.forget_all_plugins();
        });
    } else {
        with_state(|state| {
            state.plugin_manager.revoke_capabilities(target);
            state.plugin_manager.forget_plugin(target);
        });
    }
    Ok(())
}

fn fnv_hash_str(s: &str) -> String {
    use std::hash::{Hash, Hasher as _};
    let mut hasher = fnv::FnvHasher::default();
    s.hash(&mut hasher);
    format!("{:x}", hasher.finish())
}

fn sanitize_plugin_url(url: &str) -> &str {
    url.trim_end_matches(".mjs.map")
        .trim_end_matches(".mjs")
        .trim_end_matches(".json")
}

/// Cap on a fetched plugin manifest (`.json`). Manifests are small; the bound
/// stops an oversized or hostile body from being buffered in the app process.
const MAX_MANIFEST_BYTES: usize = 1024 * 1024;

/// Cap on fetched plugin code (`.mjs`). Generous for a bundled plugin; bounds
/// memory if a plugin host returns an arbitrarily large body.
const MAX_CODE_BYTES: usize = 16 * 1024 * 1024;

/// Stream `resp`'s body into memory, failing if it exceeds `max` (rejected without
/// buffering the whole body); mirrors the updater's capped download.
///
/// The one bounded body read on the plugin paths. Its callers differ only in how they decode
/// the bytes afterwards, which is why the cap sits here and not at each decode.
pub(super) async fn read_bytes_capped(
    resp: reqwest::Response,
    max: usize,
    what: &str,
) -> anyhow::Result<Vec<u8>> {
    use futures_util::StreamExt as _;
    let mut buf: Vec<u8> = Vec::new();
    let mut total = 0usize;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| anyhow::anyhow!("{what} transport failed: {e}"))?;
        total = bump_capped(total, chunk.len(), max)?;
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

/// [`read_bytes_capped`] as text, rejecting anything that is not valid UTF-8.
async fn read_body_capped(
    resp: reqwest::Response,
    max: usize,
    what: &str,
) -> anyhow::Result<String> {
    let buf = read_bytes_capped(resp, max, what).await?;
    String::from_utf8(buf).map_err(|e| anyhow::anyhow!("{what} is not valid UTF-8: {e}"))
}

/// GET `url` as text under the `max` cap; any non-2xx is an error.
async fn fetch_text_capped(
    client: &reqwest::Client,
    url: &str,
    max: usize,
    what: &str,
) -> anyhow::Result<String> {
    let resp = client.get(url).send().await?.error_for_status()?;
    read_body_capped(resp, max, what).await
}

/// Fetch a plugin's `.json` manifest. `Ok(None)` = genuinely absent (404, a
/// classic .mjs-only plugin); `Err` = a hard failure (oversized/cap, bad JSON,
/// transport, other non-2xx) that callers must surface, not fall back on.
pub(super) async fn fetch_plugin_package(
    client: &reqwest::Client,
    url: &str,
) -> anyhow::Result<Option<String>> {
    let base = sanitize_plugin_url(url);
    let json_url = format!("{base}.json");
    let resp = client.get(&json_url).send().await?;
    if manifest_absent(resp.status()) {
        return Ok(None);
    }
    let resp = resp.error_for_status()?;
    let manifest_str = read_body_capped(resp, MAX_MANIFEST_BYTES, "plugin manifest").await?;
    let _: serde_json::Value = serde_json::from_str(&manifest_str)?;
    Ok(Some(manifest_str))
}

/// A plugin's manifest as fetched, and whether the source served it.
pub(super) enum Manifest {
    Served(String),
    /// Built for a source answering 404 for the `.json`: a classic `.mjs`-only plugin.
    MadeUp(String),
}

impl Manifest {
    pub(super) fn into_string(self) -> String {
        match self {
            Self::Served(text) | Self::MadeUp(text) => text,
        }
    }
}

pub(super) struct PluginData {
    pub(super) name: String,
    pub(super) manifest: Manifest,
    pub(super) code: String,
    pub(super) hash: String,
}

pub(super) async fn fetch_plugin_data(
    client: &reqwest::Client,
    url: &str,
) -> anyhow::Result<PluginData> {
    let base = sanitize_plugin_url(url);

    // `?` propagates a hard manifest failure (cap/JSON/transport); only a
    // genuinely-absent manifest (`None`) falls back to the legacy .mjs path.
    if let Some(manifest_str) = fetch_plugin_package(client, base).await? {
        let manifest: serde_json::Value = serde_json::from_str(&manifest_str)?;
        // No "unknown" stand-in: it made every nameless plugin share one name and its data.
        let name = manifest["name"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("plugin manifest has no name"))?
            .to_string();

        let code_url = format!("{base}.mjs");
        let code = fetch_text_capped(client, &code_url, MAX_CODE_BYTES, "plugin code").await?;
        let hash = fnv_hash_str(&code);

        Ok(PluginData {
            name,
            manifest: Manifest::Served(manifest_str),
            code,
            hash,
        })
    } else {
        let fetch_url = if url.ends_with("package.json") {
            url.to_string()
        } else if !url.ends_with(".mjs") {
            format!("{base}.mjs")
        } else {
            url.to_string()
        };

        let code = fetch_text_capped(client, &fetch_url, MAX_CODE_BYTES, "plugin code").await?;

        let name = base
            .rsplit_once('/')
            .map(|(_, n)| n)
            .unwrap_or("plugin")
            .to_string();
        let manifest = serde_json::json!({"name": &name, "main": &fetch_url}).to_string();
        let hash = fnv_hash_str(&code);

        Ok(PluginData {
            name,
            manifest: Manifest::MadeUp(manifest),
            code,
            hash,
        })
    }
}

static INSTALL_CONSENTS: std::sync::LazyLock<super::install_consent::Consents> =
    std::sync::LazyLock::new(Default::default);

enum InstallError {
    Refused(super::install_consent::Refusal),
    Failed(String),
}

impl From<super::install_consent::Refusal> for InstallError {
    fn from(refusal: super::install_consent::Refusal) -> Self {
        Self::Refused(refusal)
    }
}

impl From<String> for InstallError {
    fn from(e: String) -> Self {
        Self::Failed(e)
    }
}

/// Install from `url` (`.json` manifest + `.mjs` code) from any host (stores, localhost dev). The
/// fetch precedes the consent dialog and opens nothing `plugin.fetch_package` does not already. The
/// sole writer of a plugin row: nothing is stored until the user confirms.
async fn do_plugin_install(url: String) -> Result<crate::plugins::PluginInfo, InstallError> {
    // Before the dialog: an install is enabled next, which the gate refuses, and that refusal
    // would throw away an install the user had just agreed to.
    if !with_state(|state| state.proactive_refresh_done).unwrap_or(false) {
        return Err(InstallError::Failed(
            super::admission::Closed::Gate.reason().to_string(),
        ));
    }
    let turn = INSTALL_CONSENTS.begin()?;
    let client = &*crate::state::HTTP_CLIENT;
    let PluginData {
        name,
        manifest,
        code,
        hash,
    } = fetch_plugin_data(client, &url)
        .await
        .map_err(|e| format!("{e:#}"))?;
    let manifest = manifest.into_string();
    let name = crate::plugins::name::PluginName::parse(&name).map_err(|r| r.to_string())?;
    check_luna_type(&manifest)?;

    // The user approves the bytes in hand; the write below stores these, never a second fetch.
    let answer = crate::ui::install_dialog::show_install_dialog(name.as_str(), &url, &manifest)
        .await
        .ok();
    turn.settle(answer)?;

    // Under the pass lock, as every writer of a row: the row (one actor trip; no trust write
    // queued on the actor lands between the row and the reset), then what still ran of the old
    // install. A refused or failed row write changes nothing.
    let installed = super::jsrt::in_section(move |pass| {
        let db = crate::state::db();
        let replaced = {
            let url = url.clone();
            db.call_plugins(move |pc| reinstall_guard(pc, &url))?
        };
        {
            let (url, name, manifest, code, hash) = (
                url.clone(),
                name.clone(),
                manifest.clone(),
                code,
                hash.clone(),
            );
            db.call(move |pc, sc| commit_install(pc, sc, &url, &name, &manifest, &code, &hash))?;
        }
        // The old load first; it asks and registers nothing after its trust and channels go. The
        // name installed is reset again: its load could still ask after `commit_install`. A name the
        // source no longer gives is reset too, or a later plugin under it would inherit its trust.
        super::inject::unload_current(pass, &url);
        let old = replaced.filter(|old| *old != name);
        // Every name is cleared before a failed reset is answered: the old load is already gone.
        let mut reset = Ok(());
        for owner in std::iter::once(name.clone()).chain(old) {
            let scope = CleanupScope::Plugin(owner.clone());
            let done = db.call_settings(move |sc| super::native::reset_trust(sc, &owner));
            super::native::clear_native_channels(&scope);
            reset = reset.and(done);
        }
        reset.map_err(|e| format!("DB error: {e}"))?;
        Ok::<_, String>(crate::plugins::PluginInfo {
            url,
            name: name.to_string(),
            manifest,
            hash: Some(hash),
            enabled: false,
            installed: true,
            load_error: None,
        })
    })
    .await?;

    Ok(installed?)
}

/// A reinstall turns the plugin off: refused while enabled plugins depend on the one installed at
/// `url`, as a disable is, under the name its row has now. Answers that name, if it is one.
fn reinstall_guard(
    pc: &mut rusqlite::Connection,
    url: &str,
) -> Result<Option<crate::plugins::name::PluginName>, String> {
    let Some(name) = crate::plugins::store::get_name_by_url(pc, url) else {
        return Ok(None);
    };
    match crate::plugins::store::find_enabled_dependants(pc, &name)? {
        deps if deps.is_empty() => Ok(crate::plugins::name::PluginName::parse(&name).ok()),
        deps => Err(format!(
            "Cannot reinstall '{name}': active dependants: {}",
            deps.join(", ")
        )),
    }
}

/// Refuses a luna.type other than plugin or library; none at all is fine.
pub(super) fn check_luna_type(manifest: &str) -> Result<(), String> {
    match crate::plugins::store::parse_luna_meta(manifest)? {
        Some(crate::plugins::store::LunaMeta {
            plugin_type: Some(t),
            ..
        }) if t != "plugin" && t != "library" => {
            Err(format!("Invalid luna.type '{t}' in manifest"))
        }
        _ => Ok(()), // no luna field, or no type
    }
}

/// Stores the plugin row and resets its native trust, which only a reinstall does: a decision kept
/// across it would answer for identical bytes. The row commits last; a failed reset leaves the
/// previous install as it was, and a commit failing after the reset only asks again.
fn commit_install(
    pc: &rusqlite::Connection,
    sc: &mut rusqlite::Connection,
    url: &str,
    name: &crate::plugins::name::PluginName,
    manifest: &str,
    code: &str,
    hash: &str,
) -> Result<(), String> {
    let tx = pc
        .unchecked_transaction()
        .map_err(|e| format!("DB error: {e}"))?;
    crate::plugins::store::install(&tx, url, name, manifest, code, hash)?;
    super::native::reset_trust(sc, name).map_err(|e| format!("DB error: {e}"))?;
    tx.commit().map_err(|e| format!("DB error: {e}"))
}

/// Running byte-total guard; `saturating_add` keeps a huge chunk from overflowing
/// into a pass. Mirrors the updater's download cap.
fn bump_capped(running: usize, add: usize, max: usize) -> anyhow::Result<usize> {
    let next = running.saturating_add(add);
    if next > max {
        anyhow::bail!("response exceeds the {max}-byte cap");
    }
    Ok(next)
}

/// A missing `.json` manifest (classic .mjs-only plugin) is a 404; other non-2xx
/// are real failures the caller must surface, not silently fall back on.
fn manifest_absent(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::NOT_FOUND
}

#[cfg(test)]
#[path = "../../../tests/unit/ipc/plugin/plugin_ipc.rs"]
mod tests;
