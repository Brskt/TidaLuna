mod activation;
mod admission;
mod auth_fetch;
mod download;
mod inject;
mod install;
mod install_consent;
mod jsrt;
mod lib_native;
mod native;
mod path_probe;
mod plugin_db;
mod proxy;
mod source;
mod store_move;
mod trust_prompt;
mod uninstall;
mod update;

pub(crate) use admission::page_stamp;
pub(crate) use jsrt::JS_PURGE_SDK_BLOB;
pub(crate) use jsrt::handle_jsrt_fire_and_forget;
pub(crate) use jsrt::open_plugin_gate;
pub(crate) use native::native_data_dirs;
// Token-scrub for plugin-facing response bodies; reused by the native egress path.
pub(crate) use proxy::scrub_real_tokens;

use crate::app_state::{IpcCallback, IpcMessage, with_state};
use std::sync::atomic::{AtomicBool, Ordering};

/// Scrub the PKCE codeVerifier from nativeInterface after successful auth.
/// Called from both jsrt.set_token (primary path) and proxy/token_exchange.rs oauth2/token
/// (fallback).
/// Best-effort: eval_js may fail if no renderer frame is available.
static PKCE_SCRUBBED: AtomicBool = AtomicBool::new(false);
/// Re-arm the scrub latch: the next login cycle triggers a fresh scrub.
/// Called from session_clear and session_hard_reset.
pub(super) fn reset_pkce_scrub() {
    PKCE_SCRUBBED.store(false, Ordering::SeqCst);
}

pub(crate) fn scrub_pkce_verifier() {
    if PKCE_SCRUBBED.swap(true, Ordering::SeqCst) {
        return; // already scrubbed
    }
    crate::app_state::eval_js(
        "try{delete window.nativeInterface.credentials.codeVerifier}catch(e){}",
    );
    crate::vprintln!("[AUTH]   PKCE codeVerifier scrub requested");
}

fn take_ipc_callback(query_id: i64) -> Option<IpcCallback> {
    with_state(|state| state.pending_ipc_callbacks.remove(&query_id)).flatten()
}

/// Called from `IpcQueryHandler::on_query_canceled`. The in-flight task then finds `None`
/// and returns instead of touching a callback the router has retired.
pub(crate) fn drop_ipc_callback(query_id: i64) {
    with_state(|state| state.pending_ipc_callbacks.remove(&query_id));
}

pub(crate) fn ipc_callback_ok(cb: &IpcCallback, result: &str) {
    cb.lock()
        .unwrap_or_else(|e| e.into_inner())
        .success_str(result);
}

pub(crate) fn ipc_callback_err(cb: &IpcCallback, code: i32, msg: &str) {
    cb.lock()
        .unwrap_or_else(|e| e.into_inner())
        .failure(code, msg);
}

/// Answers with `value` as JSON; a value that will not serialize fails the call as a 500.
pub(crate) fn ipc_callback_json(cb: &IpcCallback, value: &impl serde::Serialize) {
    match serde_json::to_string(value) {
        Ok(json) => ipc_callback_ok(cb, &json),
        Err(e) => ipc_callback_err(cb, 500, &e.to_string()),
    }
}

/// Answers a cache click with the state its change left, or fails it if the change never ran.
fn audio_cache_completion(callback: IpcCallback) -> crate::player::cache::Completion {
    crate::player::cache::Completion::new(move |outcome| match outcome {
        Ok(state) => ipc_callback_json(&callback, &state),
        Err(why) => ipc_callback_err(&callback, 500, why),
    })
}

pub(crate) fn handle_plugin_ipc(msg: IpcMessage, query_id: i64, callback: IpcCallback) {
    if let Some(route) = attributed_route(&msg.channel) {
        // Resolved once, before dispatch: a handler resolving again could read a capability the
        // gate accepted and eviction has since dropped, turning an allowed call unattributed after
        // the fact.
        let caller = crate::ipc::caller::Caller::resolve(&msg);
        let attributed = match caller.require_plugin() {
            Ok(attributed) => attributed,
            Err((code, why)) => {
                // Gated: the caller gets the refusal as its reply; the log is not the only
                // channel, and an ungated write would let a refused-call loop drive the disk
                // from the renderer.
                crate::vprintln!("[IPC] Refused {}: {why}", msg.channel);
                ipc_callback_err(&callback, code, why);
                return;
            }
        };
        match route {
            AttributedRoute::Storage => {
                plugin_db::handle_plugin_storage(msg, attributed.plugin_id, callback);
            }
            AttributedRoute::Fetch => {
                auth_fetch::handle_plugin_fetch(&msg, &attributed, query_id, callback);
            }
            AttributedRoute::RegisterNative => {
                native::handle_register_native(&msg, attributed.plugin_id, callback);
            }
        }
        return;
    }
    match msg.channel.as_str() {
        "tidal.fetch" => {
            auth_fetch::handle_tidal_fetch(&msg, query_id, callback);
        }
        "player.parse_dash" => {
            let xml = msg.arg(0);
            match crate::player::dash::parse_dash_mpd(xml) {
                Ok(manifest) => {
                    let json = serde_json::to_string(&manifest).unwrap_or_else(|_| "null".into());
                    ipc_callback_ok(&callback, &json);
                }
                Err(e) => {
                    ipc_callback_err(&callback, parse_dash_failure_code(&e), &format!("{e:#}"))
                }
            }
        }
        "plugin.download" => {
            download::handle_download(&msg, callback);
        }
        "proxy.fetch" => {
            proxy::handle_proxy_fetch_dispatch(&msg, query_id, callback);
        }
        "proxy.head" => {
            proxy::handle_proxy_head_dispatch(&msg, query_id, callback);
        }
        "__Luna.clipboardWriteText" => {
            lib_native::handle_clipboard_write_text(&msg, callback);
        }
        "__Luna.openExternal" => {
            lib_native::handle_open_external(&msg, callback);
        }
        "__Luna.sendToRender" => {
            lib_native::handle_send_to_render(&msg, callback);
        }
        "__Luna.showMessageBox" => {
            lib_native::handle_show_message_box(&msg, callback);
        }
        "__Luna.showErrorBox" => {
            lib_native::handle_show_error_box(&msg, callback);
        }
        "__Luna.showOpenDialog" => {
            lib_native::handle_show_open_dialog(&msg, callback);
        }
        "__Luna.showSaveDialog" => {
            lib_native::handle_show_save_dialog(&msg, callback);
        }
        ch if ch.starts_with("__LunaNative.") => {
            native::handle_native_call(&msg, callback);
        }
        "plugin.fetch_package" => {
            source::handle_plugin_fetch_package(&msg, callback);
        }
        "plugin.install" => {
            install::handle_plugin_install(&msg, callback);
        }
        "plugin.migrate_base_url" => {
            store_move::handle_plugin_migrate_base_url(&msg, callback);
        }
        "plugin.check_hash" => {
            source::handle_plugin_check_hash(&msg, query_id, callback);
        }
        "plugin.update" => {
            update::handle_plugin_update(&msg, callback);
        }
        "plugin.update_all" => {
            update::handle_plugin_update_all(callback);
        }
        "plugin.enable" => {
            activation::handle_plugin_enable(&msg, callback);
        }
        "plugin.disable" => {
            activation::handle_plugin_disable(&msg, callback);
        }
        "jsrt.load_plugins" => {
            jsrt::handle_jsrt_load_plugins(callback);
        }
        "updater.check" => {
            crate::updater::handle_updater_check(callback);
        }
        "updater.download" => {
            crate::updater::handle_updater_download(&msg, callback);
        }
        "updater.status" => {
            crate::updater::handle_updater_status(callback);
        }
        "settings.video_codec_state" => {
            ipc_callback_ok(&callback, &crate::video::codec::state_json());
        }
        // Live: the supervisor follows once free (a download under way finishes first) and answers
        // with its snapshot after the step that took this click, holding the query that long. A
        // decode already running is not interrupted mid-frame.
        "settings.set_video_codec" => {
            let enabled = msg.args.first().and_then(|v| v.as_bool()).unwrap_or(false);
            crate::state::db().post(move |_, conn| {
                crate::settings::save_video_codec_enabled(conn, enabled);
            });
            let answered = crate::video::codec::set_enabled(enabled);
            crate::vprintln!("[VIDEO]   Codec set to {enabled}");
            crate::state::rt_handle().spawn(async move {
                match answered.await {
                    Ok(view) => ipc_callback_json(&callback, &view),
                    Err(_) => {
                        ipc_callback_err(&callback, 503, "the video codec service has stopped")
                    }
                }
            });
        }
        // Off the UI thread: a clear or a lowered cap holds the cache lock while it deletes.
        "settings.audio_cache_state" => {
            crate::state::rt_handle().spawn_blocking(move || {
                let Ok(mut cache) = crate::state::AUDIO_CACHE.lock() else {
                    ipc_callback_err(&callback, 500, "audio cache lock poisoned");
                    return;
                };
                let state = cache.state();
                drop(cache);
                ipc_callback_json(&callback, &state);
            });
        }
        // Invoked rather than sent: the page lets go of the click on this reply, the state taken
        // once the change is applied, and on nothing else.
        "settings.set_audio_cache_enabled" => {
            let enabled = msg.args.first().and_then(|v| v.as_bool()).unwrap_or(true);
            crate::state::db().post(move |_, conn| {
                crate::settings::save_audio_cache_enabled(conn, enabled);
            });
            crate::player::cache::queue_change(
                crate::player::cache::Request::Switch,
                move |cache| {
                    cache.set_enabled(enabled);
                    Ok(())
                },
                Some(audio_cache_completion(callback)),
            );
            crate::vprintln!("[CACHE]  Cache set to {enabled}");
        }
        "settings.set_audio_cache_size" => {
            use crate::player::cache::{MAX_CAPACITY_GB, MIN_CAPACITY_GB, capacity_bytes};
            // Read as f64 like the crossfade: `as_u64` rejects 4.0 as readily as 4.5. A value
            // the slider cannot send is refused rather than guessed at.
            let Some(gb) = msg
                .args
                .first()
                .and_then(|v| v.as_f64())
                .filter(|gb| gb.is_finite())
                .map(|gb| {
                    gb.round()
                        .clamp(MIN_CAPACITY_GB as f64, MAX_CAPACITY_GB as f64)
                        as u8
                })
            else {
                ipc_callback_err(&callback, 400, "audio cache size is not a number");
                return;
            };
            crate::state::db().post(move |_, conn| {
                crate::settings::save_audio_cache_gb(conn, gb);
            });
            crate::player::cache::queue_resize(
                capacity_bytes(gb),
                Some(audio_cache_completion(callback)),
            );
            crate::vprintln!("[CACHE]  Cap set to {gb} GB");
        }
        "settings.plugin_update_at_boot" => {
            crate::state::db().post(move |_, settings| {
                let on = crate::settings::load_plugin_update_at_boot(settings);
                ipc_callback_ok(&callback, &on.to_string());
            });
        }
        // The reply carries the repairs the user has not been told of (settings.db, plugins.db),
        // `null` when there are none; they stay in their stores until `settings.store_repair_ack`.
        "settings.store_repair" => {
            crate::state::db().post(move |plugins, settings| {
                ipc_callback_json(&callback, &crate::db::pending_repairs(plugins, settings));
            });
        }
        _ => {
            plugin_db::handle_plugin_db(msg, callback);
        }
    }
}

/// Channels whose handler acts on the caller's identity: serving one unattributed would mean
/// acting on a name the caller chose.
///
/// A channel belongs here once every legitimate route to it carries the capability. The wrapper's
/// own shims always did; `@luna/lib` did not, its `invoke` closing over the app bundle's scope
/// rather than the calling plugin's, which is why `registerNative` took its identity from an
/// argument for so long. It is listed now that `transpile::lower_import` binds that import to a
/// per-plugin copy. A route still arriving unattributed is refused.
///
/// Matched exactly, not by prefix: `plugin.fetch_package` installs a plugin before one exists to
/// hold a capability.
fn attributed_route(channel: &str) -> Option<AttributedRoute> {
    if channel.starts_with("plugin.storage.") {
        Some(AttributedRoute::Storage)
    } else if channel == "plugin.fetch" {
        Some(AttributedRoute::Fetch)
    } else if channel == "__Luna.registerNative" {
        Some(AttributedRoute::RegisterNative)
    } else {
        None
    }
}

/// The handlers [`attributed_route`] sends a channel to, each given the [`Attributed`] caller.
///
/// [`Attributed`]: crate::ipc::caller::Attributed
enum AttributedRoute {
    Storage,
    Fetch,
    RegisterNative,
}

/// The failure code `player.parse_dash` answers `err` with.
///
/// A profile no decoder here handles is not a malformed manifest, and the code is all the
/// renderer gets to tell them apart: one earns the listener a message naming the quality to
/// pick, the other earns silence. 415 mirrors HTTP's Unsupported Media Type.
fn parse_dash_failure_code(err: &anyhow::Error) -> i32 {
    match err.downcast_ref::<crate::player::dash::UndecodableProfile>() {
        Some(_) => 415,
        None => 400,
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/ipc/plugin.rs"]
mod tests;
