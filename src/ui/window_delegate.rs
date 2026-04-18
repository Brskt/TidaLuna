use crate::app_state::with_state;
use crate::ipc::window::notify_window_state;
use crate::ui::flush::run_flush_batch;
use cef::*;
use std::cell::{OnceCell, RefCell};

// --- Win32 frameless subclass ---

#[cfg(target_os = "windows")]
unsafe extern "system" fn frameless_subclass_proc(
    hwnd: windows_sys::Win32::Foundation::HWND,
    msg: u32,
    wparam: windows_sys::Win32::Foundation::WPARAM,
    lparam: windows_sys::Win32::Foundation::LPARAM,
    _uid: usize,
    _ref_data: usize,
) -> windows_sys::Win32::Foundation::LRESULT {
    use windows_sys::Win32::UI::Shell::DefSubclassProc;
    use windows_sys::Win32::UI::WindowsAndMessaging::*;

    unsafe {
        match msg {
            WM_NCCALCSIZE if wparam != 0 => {
                let params = &*(lparam as *const NCCALCSIZE_PARAMS);
                let proposed = params.rgrc[0];
                DefWindowProcW(hwnd, msg, wparam, lparam);
                let params = &mut *(lparam as *mut NCCALCSIZE_PARAMS);

                if IsZoomed(hwnd) != 0 {
                    use windows_sys::Win32::Graphics::Gdi::*;
                    let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
                    let mut mi: MONITORINFO = core::mem::zeroed();
                    mi.cbSize = core::mem::size_of::<MONITORINFO>() as u32;
                    if GetMonitorInfoW(monitor, &mut mi) != 0 {
                        params.rgrc[0] = mi.rcWork;
                    } else {
                        params.rgrc[0] = proposed;
                    }
                } else {
                    params.rgrc[0] = proposed;
                }
                0
            }
            WM_SYSCOMMAND
                if matches!(
                    (wparam & 0xFFF0) as u32,
                    SC_MINIMIZE | SC_MAXIMIZE | SC_RESTORE
                ) =>
            {
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_COMMAND => {
                let id = (wparam & 0xFFFF) as u32;
                match id {
                    0 => {
                        crate::app_state::eval_js(crate::platform::js_actions::PLAY_PREV);
                    }
                    1 => {
                        crate::app_state::eval_js(crate::platform::js_actions::PLAY_PAUSE);
                    }
                    2 => {
                        crate::app_state::eval_js(crate::platform::js_actions::PLAY_NEXT);
                    }
                    _ => return DefSubclassProc(hwnd, msg, wparam, lparam),
                }
                0
            }
            _ => DefSubclassProc(hwnd, msg, wparam, lparam),
        }
    }
}

// --- Window Delegate ---

fn load_init_window_state() -> crate::settings::WindowState {
    crate::state::db().call_settings(crate::settings::load_window_state)
}

wrap_window_delegate! {
    pub(super) struct TidalWindowDelegate {
        browser_view: RefCell<Option<BrowserView>>,
        init_window_state: OnceCell<crate::settings::WindowState>,
    }
    impl ViewDelegate {
        fn preferred_size(&self, _view: Option<&mut View>) -> Size {
            let ws = self.init_window_state.get_or_init(load_init_window_state);
            Size {
                width: ws.width as i32,
                height: ws.height as i32,
            }
        }
    }
    impl PanelDelegate {}
    impl WindowDelegate {
        fn initial_bounds(&self, _window: Option<&mut Window>) -> cef::Rect {
            let ws = self.init_window_state.get_or_init(load_init_window_state);
            if ws.has_position() && !ws.maximized {
                cef::Rect {
                    x: ws.x,
                    y: ws.y,
                    width: ws.width as i32,
                    height: ws.height as i32,
                }
            } else {
                cef::Rect {
                    x: 0,
                    y: 0,
                    width: ws.width as i32,
                    height: ws.height as i32,
                }
            }
        }
        fn initial_show_state(&self, _window: Option<&mut Window>) -> cef::ShowState {
            let ws = self.init_window_state.get_or_init(load_init_window_state);
            if ws.maximized {
                cef::ShowState::MAXIMIZED
            } else {
                cef::ShowState::NORMAL
            }
        }
        fn is_frameless(&self, _window: Option<&mut Window>) -> ::std::os::raw::c_int {
            1
        }
        fn can_resize(&self, _window: Option<&mut Window>) -> ::std::os::raw::c_int {
            1
        }
        fn can_maximize(&self, _window: Option<&mut Window>) -> ::std::os::raw::c_int {
            1
        }
        fn can_minimize(&self, _window: Option<&mut Window>) -> ::std::os::raw::c_int {
            1
        }
        fn on_window_created(&self, window: Option<&mut Window>) {
            let bv = self.browser_view.borrow();
            let (Some(window), Some(bv)) = (window, bv.as_ref()) else {
                return;
            };
            let mut view = View::from(bv);
            window.add_child_view(Some(&mut view));

            if let Some(mut icon) = image_create() {
                let png_data = include_bytes!("../../tidaluna.png");
                icon.add_png(1.0, Some(png_data));
                window.set_window_icon(Some(&mut icon));
                window.set_window_app_icon(Some(&mut icon));
            }

            // CEF Views creates X11 windows directly and ignores Chromium's
            // --class switch, so the window has no WM_CLASS by default and
            // GNOME cannot match the installed .desktop entry. Set it via
            // XSetClassHint here.
            #[cfg(target_os = "linux")]
            {
                let xid = window.window_handle();
                if xid != 0 {
                    crate::platform::x11_class::set_wm_class(
                        xid,
                        crate::platform::desktop_entry::WM_CLASS,
                    );
                }
            }

            #[cfg(target_os = "windows")]
            {
                use windows_sys::Win32::UI::Shell::SetWindowSubclass;
                use windows_sys::Win32::UI::WindowsAndMessaging::*;
                let hwnd = window.window_handle().0 as *mut core::ffi::c_void;
                if !hwnd.is_null() {
                    unsafe {
                        let style = GetWindowLongPtrW(hwnd as _, GWL_STYLE) as u32;
                        let style = (style & !WS_POPUP)
                            | WS_THICKFRAME
                            | WS_MINIMIZEBOX
                            | WS_MAXIMIZEBOX
                            | WS_SYSMENU;
                        SetWindowLongPtrW(hwnd as _, GWL_STYLE, style as isize);
                        SetWindowSubclass(hwnd, Some(frameless_subclass_proc), 1, 0);
                        SetWindowPos(
                            hwnd,
                            core::ptr::null_mut(),
                            0,
                            0,
                            0,
                            0,
                            SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER,
                        );
                    }
                }
            }

            {
                #[cfg(target_os = "windows")]
                let hwnd = Some(window.window_handle().0 as *mut std::ffi::c_void);
                #[cfg(not(target_os = "windows"))]
                let hwnd = None;

                if let Some(mc) = crate::platform::media_controls::OsMediaControls::new(hwnd) {
                    with_state(|state| {
                        state.media_controls = Some(mc);
                    });
                }
            }

            window.show();

            #[cfg(target_os = "windows")]
            {
                let hwnd = window.window_handle().0 as windows_sys::Win32::Foundation::HWND;
                if let Some(tb) = crate::platform::thumbbar::ThumbBar::new(hwnd) {
                    with_state(|state| {
                        state.thumbbar = Some(tb);
                    });
                }
            }
        }
        fn on_window_destroyed(&self, _window: Option<&mut Window>) {
            with_state(|state| state.force_quit = false);
            crate::platform::tray::destroy_tray();
            *self.browser_view.borrow_mut() = None;
        }
        fn on_window_bounds_changed(
            &self,
            window: Option<&mut Window>,
            new_bounds: Option<&cef::Rect>,
        ) {
            let (Some(window), Some(bounds)) = (window, new_bounds) else {
                return;
            };
            if window.is_minimized() == 1 {
                return;
            }
            let maximized = window.is_maximized() == 1;
            let batch = with_state(|state| {
                state.pending_window_save = Some(crate::settings::WindowState {
                    x: bounds.x,
                    y: bounds.y,
                    width: bounds.width as u32,
                    height: bounds.height as u32,
                    maximized,
                });
                if !state.window_save_scheduled {
                    state.window_save_scheduled = true;
                    schedule_window_save();
                }
                notify_window_state(state, maximized, false)
            });
            if let Some(batch) = batch {
                run_flush_batch(batch);
            }
        }
        fn can_close(&self, _window: Option<&mut Window>) -> i32 {
            // Read without consuming - CEF may call can_close multiple times
            // during the browser close handshake. Cleared in on_window_destroyed.
            let force_quit = with_state(|state| state.force_quit).unwrap_or(false);
            if !force_quit {
                let close_to_tray = with_state(|state| state.close_to_tray).unwrap_or(false);
                if close_to_tray
                    && crate::platform::tray::has_tray()
                    && let Some(window) = _window
                {
                    window.hide();
                    return 0;
                }
            }

            let pending_ws = with_state(|state| state.pending_window_save.take()).flatten();
            if let Some(ws) = pending_ws {
                crate::state::db().call_settings(move |sc| {
                    if ws.maximized {
                        crate::settings::save_maximized(sc, true);
                    } else {
                        crate::settings::save_window_state(sc, &ws);
                    }
                });
            }
            let bv = self.browser_view.borrow();
            let Some(bv) = bv.as_ref() else { return 1; };
            if let Some(browser) = bv.browser() {
                browser.host().unwrap().try_close_browser()
            } else {
                1
            }
        }
    }
}

// --- Browser View Delegate ---

wrap_browser_view_delegate! {
    pub(super) struct TidalBrowserViewDelegate {
        _p: u8,
    }
    impl ViewDelegate {}
    impl BrowserViewDelegate {}
}

// --- Window Save Task ---

fn schedule_window_save() {
    let mut task = WindowSaveTask::new(0);
    post_delayed_task(ThreadId::UI, Some(&mut task), 500);
}

wrap_task! {
    struct WindowSaveTask {
        _p: u8,
    }
    impl Task {
        fn execute(&self) {
            let pending_ws = with_state(|state| {
                state.window_save_scheduled = false;
                state.pending_window_save.take()
            })
            .flatten();
            if let Some(ws) = pending_ws {
                crate::state::db().call_settings(move |sc| {
                    if ws.maximized {
                        crate::settings::save_maximized(sc, true);
                    } else {
                        crate::settings::save_window_state(sc, &ws);
                    }
                });
            }
        }
    }
}
