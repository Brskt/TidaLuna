use crate::app_state::{AppState, IpcMessage, open_in_os, toggle_devtools, with_state};
use crate::ui::flush::{FlushBatch, run_flush_batch, take_flush_batch};
use crate::ui::menu::{HamburgerMenuDelegate, MenuCommand};
use cef::*;

pub(crate) fn handle_window_ipc(msg: &IpcMessage) {
    match msg.channel.as_str() {
        "window.close" => {
            let browser = with_state(|state| state.browser.clone());
            if let Some(window) = get_cef_window(browser.flatten()) {
                window.close();
            } else {
                quit_message_loop();
            }
        }
        "window.minimize" => {
            let browser = with_state(|state| state.browser.clone());
            if let Some(window) = get_cef_window(browser.flatten()) {
                #[cfg(target_os = "windows")]
                {
                    use windows_sys::Win32::UI::WindowsAndMessaging::*;
                    let hwnd = window.window_handle().0 as windows_sys::Win32::Foundation::HWND;
                    if !hwnd.is_null() {
                        unsafe {
                            PostMessageW(hwnd, WM_SYSCOMMAND, SC_MINIMIZE as usize, 0);
                        }
                    }
                }
                #[cfg(not(target_os = "windows"))]
                window.minimize();
            }
        }
        "window.maximize" | "window.unmaximize" => {
            let browser = with_state(|state| state.browser.clone()).flatten();
            if let Some(window) = get_cef_window(browser) {
                let maximized = window.is_maximized() == 1;
                #[cfg(target_os = "windows")]
                {
                    use windows_sys::Win32::UI::WindowsAndMessaging::*;
                    let hwnd = window.window_handle().0 as windows_sys::Win32::Foundation::HWND;
                    if !hwnd.is_null() {
                        let cmd = if maximized { SC_RESTORE } else { SC_MAXIMIZE };
                        unsafe {
                            PostMessageW(hwnd, WM_SYSCOMMAND, cmd as usize, 0);
                        }
                    }
                }
                #[cfg(not(target_os = "windows"))]
                {
                    if maximized {
                        window.restore();
                    } else {
                        window.maximize();
                    }
                }
                let batch = with_state(|state| notify_window_state(state, !maximized, false));
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

            let browser = with_state(|state| state.browser.clone()).flatten();
            if let Some(window) = get_cef_window(browser) {
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
                    #[cfg(target_os = "windows")]
                    {
                        menu.add_separator();
                        let vs_enabled = crate::state::db()
                            .call_settings(|conn| crate::settings::load_volume_sync(conn));
                        let label = if vs_enabled {
                            "✓ Sync Volume with OS"
                        } else {
                            "   Sync Volume with OS"
                        };
                        menu.add_item(
                            MenuCommand::VolumeSync as i32,
                            Some(&CefString::from(label)),
                        );
                    }
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
            if let Some(url) = msg.args.first().and_then(|v| v.as_str()) {
                open_in_os(url);
            }
        }
        "window.navigate_self" => {
            if let Some(url) = msg.args.first().and_then(|v| v.as_str()) {
                crate::vprintln!("[AUTH]   navigate_self → {}", &url[..url.len().min(120)]);
                let browser = with_state(|state| state.browser.clone());
                if let Some(Some(browser)) = browser
                    && let Some(frame) = browser.main_frame()
                {
                    let cef_url = CefString::from(url);
                    frame.load_url(Some(&cef_url));
                }
            }
        }
        "web.loaded" => {
            crate::ui::proactive_refresh::trigger_if_needed();
        }
        "settings.close_to_tray" => {
            crate::vprintln!("[TRAY]   IPC settings.close_to_tray received");
            let enabled = msg.args.first().and_then(|v| v.as_bool()).unwrap_or(false);
            crate::state::db().call_settings(move |conn| {
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
                    crate::state::db().call_settings(|conn| {
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
                let browser = with_state(|state| state.browser.clone()).flatten();
                if let Some(window) = get_cef_window(browser) {
                    window.show();
                }
            }
        }
        "updater.apply" => {
            crate::updater::handle_updater_apply(msg);
        }
        "updater.dismiss" => {
            crate::updater::handle_updater_dismiss(msg);
        }
        "updater.set_auto_check" => {
            let enabled = msg.args.first().and_then(|v| v.as_bool()).unwrap_or(true);
            crate::state::db().call_settings(move |conn| {
                crate::settings::save_update_auto_check(conn, enabled);
            });
            crate::vprintln!("[UPDATER] Auto-check set to {enabled}");
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

pub(crate) fn get_cef_window(mut browser: Option<Browser>) -> Option<Window> {
    let bv = browser_view_get_for_browser(browser.as_mut())?;
    bv.window()
}
