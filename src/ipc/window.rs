use crate::app_state::{AppState, IpcMessage, open_in_os, toggle_devtools, with_state};
use crate::ui::app_window::AppWindow;
use crate::ui::flush::{FlushBatch, run_flush_batch, take_flush_batch};
use crate::ui::menu::{HamburgerMenuDelegate, MenuCommand};
use cef::*;

pub(crate) fn handle_window_ipc(msg: &IpcMessage) {
    match msg.channel.as_str() {
        "window.close" => {
            if let Some(window) = AppWindow::current() {
                window.close();
            } else {
                quit_message_loop();
            }
        }
        "window.minimize" => {
            if let Some(window) = AppWindow::current() {
                window.minimize();
            }
        }
        "window.maximize" | "window.unmaximize" => {
            if let Some(window) = AppWindow::current() {
                let was_max = window.is_maximized();
                if was_max {
                    window.restore();
                } else {
                    window.maximize();
                }
                let batch = with_state(|state| notify_window_state(state, !was_max, false));
                if let Some(batch) = batch {
                    run_flush_batch(batch);
                }
            }
        }
        "window.devtools" => {
            toggle_devtools();
        }
        "menu.clicked" => {
            let x = msg.args.first().and_then(|v| v.as_i64()).unwrap_or(0) as i32;
            let y = msg.args.get(1).and_then(|v| v.as_i64()).unwrap_or(0) as i32;

            let cache_label = if let Ok(cache) = crate::state::AUDIO_CACHE.lock() {
                let mb = cache.total_size() as f64 / (1024.0 * 1024.0);
                format!("Clear Cache ({mb:.0} MB)")
            } else {
                "Clear Cache".to_string()
            };

            if let Some(window) = AppWindow::current() {
                let mut delegate = HamburgerMenuDelegate::new(0);
                if let Some(mut menu) = menu_model_create(Some(&mut delegate)) {
                    menu.add_item(
                        MenuCommand::PlayPause as i32,
                        Some(&CefString::from("Play / Pause")),
                    );
                    menu.add_item(MenuCommand::Next as i32, Some(&CefString::from("Next")));
                    menu.add_item(MenuCommand::Prev as i32, Some(&CefString::from("Previous")));
                    menu.add_item(MenuCommand::Stop as i32, Some(&CefString::from("Stop")));
                    menu.add_separator();
                    menu.add_item(
                        MenuCommand::Settings as i32,
                        Some(&CefString::from("Settings")),
                    );
                    menu.add_item(
                        MenuCommand::ClearCache as i32,
                        Some(&CefString::from(cache_label.as_str())),
                    );
                    menu.add_item(
                        MenuCommand::OpenData as i32,
                        Some(&CefString::from("Open Data Folder")),
                    );
                    menu.add_item(
                        MenuCommand::DevTools as i32,
                        Some(&CefString::from("DevTools (F12)")),
                    );
                    menu.add_separator();
                    menu.add_item(
                        MenuCommand::About as i32,
                        Some(&CefString::from("About TidaLunar")),
                    );
                    menu.add_separator();
                    menu.add_item(
                        MenuCommand::Logout as i32,
                        Some(&CefString::from("Log Out")),
                    );
                    menu.add_item(MenuCommand::Exit as i32, Some(&CefString::from("Exit")));

                    let client = window.client_area_bounds_in_screen();
                    let screen_point = Point {
                        x: client.x + x,
                        y: client.y + y,
                    };
                    window.show_menu(
                        Some(&mut menu),
                        Some(&screen_point),
                        MenuAnchorPosition::TOPLEFT,
                    );
                }
            }
        }
        "window.drag" => {
            // Drag is handled by CSS -webkit-app-region: drag
            // + DragHandler forwarding to Window::set_draggable_regions.
        }
        "window.open_url" => {
            let url = msg.arg(0);
            if !url.is_empty() {
                if crate::app_state::is_safe_open_url(url) {
                    open_in_os(url);
                } else {
                    crate::vprintln!("[IPC]    Blocked window.open_url: not https");
                }
            }
        }
        "window.navigate_self" => {
            let url = msg.arg(0);
            if !url.is_empty() {
                let kind = crate::ui::nav::PageKind::classify(url);
                let allowed = crate::app_state::is_safe_open_url(url)
                    && matches!(kind, crate::ui::nav::PageKind::AuthHost);
                if allowed {
                    crate::vprintln!(
                        "[AUTH]   navigate_self -> {}",
                        crate::util::truncate_str(&crate::util::redact_url_query(url), 120)
                    );
                    let browser = with_state(|state| state.browser.clone());
                    if let Some(Some(browser)) = browser
                        && let Some(frame) = browser.main_frame()
                    {
                        let cef_url = CefString::from(url);
                        frame.load_url(Some(&cef_url));
                    }
                } else {
                    crate::vprintln!("[AUTH]   Blocked navigate_self: not an auth host");
                }
            }
        }
        "web.loaded" => {
            crate::ui::proactive_refresh::trigger_if_needed();
        }
        "settings.close_to_tray" => {
            crate::vprintln!("[TRAY]   IPC settings.close_to_tray received");
            let enabled = msg.args.first().and_then(|v| v.as_bool()).unwrap_or(false);
            crate::state::db().post(move |_, conn| {
                crate::settings::save_close_to_tray(conn, enabled);
            });
            if enabled {
                let created = if !crate::platform::tray::has_tray() {
                    crate::platform::tray::create_tray()
                } else {
                    true
                };
                with_state(|state| {
                    state.close_to_tray = created;
                });
                if !created {
                    crate::state::db().post(|_, conn| {
                        crate::settings::save_close_to_tray(conn, false);
                    });
                    crate::app_state::eval_js(
                        "if(typeof window.__LUNAR_SET_CLOSE_TO_TRAY__==='function')\
                         window.__LUNAR_SET_CLOSE_TO_TRAY__(false);",
                    );
                }
            } else {
                with_state(|state| {
                    state.close_to_tray = false;
                });
                crate::platform::tray::destroy_tray();
                if let Some(window) = AppWindow::current() {
                    window.show();
                }
            }
        }
        #[cfg(target_os = "windows")]
        "settings.volume_sync" => {
            let enabled = msg.args.first().and_then(|v| v.as_bool()).unwrap_or(true);
            crate::state::db().post(move |_, conn| {
                crate::settings::save_volume_sync(conn, enabled);
            });
            crate::app_state::with_state(|state| {
                let _ = state.player.set_volume_sync(enabled);
            });
            crate::vprintln!("[PLAYER] Volume sync set to {enabled}");
        }
        #[cfg(target_os = "windows")]
        "settings.asio" => {
            // Persist the ASIO toggle (the mode switch itself rides on `player.devices.set`;
            // this only saves the preference, re-seeded into the frontend on next boot).
            let enabled = msg.args.first().and_then(|v| v.as_bool()).unwrap_or(false);
            crate::state::db().post(move |_, conn| {
                crate::settings::save_asio(conn, enabled);
            });
            crate::vprintln!("[PLAYER] ASIO mode persisted: {enabled}");
        }
        #[cfg(target_os = "windows")]
        "settings.exclusive" => {
            // Persist the exclusive-WASAPI toggle (the mode switch rides on
            // `player.devices.set`; this only saves the preference, re-seeded on next boot).
            let enabled = msg.args.first().and_then(|v| v.as_bool()).unwrap_or(false);
            crate::state::db().post(move |_, conn| {
                crate::settings::save_exclusive(conn, enabled);
            });
            crate::vprintln!("[PLAYER] Exclusive WASAPI mode persisted: {enabled}");
        }
        "updater.apply" => {
            crate::updater::handle_updater_apply(msg);
        }
        "updater.dismiss" => {
            crate::updater::handle_updater_dismiss(msg);
        }
        "updater.cancel" => {
            crate::updater::handle_updater_cancel();
        }
        "updater.set_auto_check" => {
            let enabled = msg.args.first().and_then(|v| v.as_bool()).unwrap_or(true);
            crate::state::db().post(move |_, conn| {
                crate::settings::save_update_auto_check(conn, enabled);
            });
            crate::vprintln!("[UPDATER] Auto-check set to {enabled}");
        }
        "updater.set_channel" => {
            // save_update_channel normalizes anything but "dev" to "stable", and
            // `from_setting` maps the same string the same way. The value handed to the
            // updater is the one a later check will be compared against; the two
            // normalizations have to agree.
            let channel = msg.arg(0).to_string();
            let now = crate::updater::UpdateChannel::from_setting(&channel);
            crate::state::db().post(move |_, conn| {
                crate::settings::save_update_channel(conn, &channel);
            });
            crate::updater::channel_changed(now);
            crate::vprintln!("[UPDATER] Channel set to {}", msg.arg(0));
        }
        "settings.set_log_level" => {
            let level = msg
                .args
                .first()
                .and_then(|v| v.as_u64())
                .unwrap_or(0)
                .min(crate::logging::MAX_LOG_LEVEL as u64) as u8;
            crate::state::db().post(move |_, conn| {
                crate::settings::save_log_level(conn, level);
            });
            crate::logging::set_log_level(level);
            // Push the effective (env-folded) level to the renderer for the player.dbg
            // gate to match; the UI can't compute the env floor itself.
            crate::app_state::eval_js(&format!(
                "window.__TIDALUNAR_LOG_LEVEL__ = {};",
                crate::logging::log_level()
            ));
            crate::vprintln!("[LOGGING] Log level set to {level}");
        }
        "settings.set_console" => {
            let enabled = msg.args.first().and_then(|v| v.as_bool()).unwrap_or(false);
            crate::state::db().post(move |_, conn| {
                crate::settings::save_console(conn, enabled);
            });
            crate::vprintln!("[LOGGING] Console window set to {enabled} (applies on restart)");
        }
        // Not Windows-gated: crossfade rides the shared cpal path, which every
        // platform uses. The exclusive and ASIO backends ignore it.
        // One channel carrying BOTH values, because what the player needs is a
        // function of the two: zero seconds when the switch is off. Separate
        // channels could not compute it without reading back whichever value did
        // not just change.
        "settings.set_crossfade" => {
            let enabled = msg.args.first().and_then(|v| v.as_bool()).unwrap_or(false);
            // The contract is whole seconds, 0 to 12, and the UI refuses anything
            // else. This is the last line, not the enforcement: read as f64 because
            // `as_u64` is syntactic and rejects 6.0 as readily as 6.5, whose
            // fallback would store "off" while the UI still showed a duration.
            // Rounding an out-of-contract value beats silently disabling the
            // feature.
            let secs = msg
                .args
                .get(1)
                .and_then(|v| v.as_f64())
                .filter(|s| s.is_finite() && *s >= 0.0)
                .map(|s| {
                    s.round()
                        .min(crate::player::crossfade::MAX_CROSSFADE_SECS as f64)
                        as u8
                })
                .unwrap_or(0);
            crate::state::db().post(move |_, conn| {
                crate::settings::save_crossfade_enabled(conn, enabled);
                crate::settings::save_crossfade_secs(conn, secs);
            });
            let effective = if enabled { secs } else { 0 };
            crate::app_state::with_state(|state| {
                let _ = state.player.set_crossfade_secs(effective);
            });
            crate::vprintln!("[PLAYER] Crossfade {enabled}, {secs}s (effective {effective}s)");
        }
        "settings.open_logs_dir" => {
            let dir = crate::state::cache_data_dir().join("logs");
            let _ = std::fs::create_dir_all(&dir);
            open_in_os(dir);
        }
        "settings.open_log_file" => {
            open_in_os(crate::state::cache_data_dir().join("console.log"));
        }
        _ => {}
    }
}

pub(crate) fn notify_window_state(
    state: &mut AppState,
    maximized: bool,
    fullscreen: bool,
) -> FlushBatch {
    let js = format!(
        "if (window.__TIDAL_CALLBACKS__ && window.__TIDAL_CALLBACKS__.window) \
         {{ window.__TIDAL_CALLBACKS__.window.updateState({maximized}, {fullscreen}); }}"
    );
    state.pending_misc_js.push(js);
    take_flush_batch(state)
}
