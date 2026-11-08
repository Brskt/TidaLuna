use crate::app_state::with_state;
use cef::*;

pub(crate) struct AppWindow {
    cef: Window,
}

impl AppWindow {
    /// Resolve the window owning the given browser. Prefer this over
    /// `current()` from inside CEF callbacks that already receive a browser
    /// handle - it preserves the event's browser identity (popup vs main vs
    /// devtools), which `current()` cannot.
    pub(crate) fn from_browser(browser: Option<&mut Browser>) -> Option<Self> {
        let mut owned = browser.cloned();
        let bv = browser_view_get_for_browser(owned.as_mut())?;
        bv.window().map(|cef| Self { cef })
    }

    /// Resolve the main app window from `AppState`. Use this from IPC handlers
    /// and other contexts that have no browser in hand. Logs at vprintln2 when
    /// resolution fails.
    pub(crate) fn current() -> Option<Self> {
        let mut browser = with_state(|state| state.browser.clone()).flatten();
        if let Some(window) = Self::from_browser(browser.as_mut()) {
            return Some(window);
        }
        crate::vprintln2!("[WINDOW] AppWindow::current: no BrowserView/Window");
        None
    }

    /// Bring the running window forward, wherever the request came from: a
    /// duplicate launch, a deep link, the relaunch CEF reports itself, or the
    /// tray icon. `show` answers for a window hidden to the tray, `activate` for
    /// one that sits behind another application.
    ///
    /// `restore` is asked of a minimized window only. It is not idle otherwise.
    /// On all three platforms it also un-maximizes, and the copies that called it
    /// unguarded took a maximized window away on every raise. That loss outlived
    /// the session, `on_window_bounds_changed` persisting the smaller bounds half
    /// a second later.
    pub(crate) fn raise_current() {
        if let Some(window) = Self::current() {
            if window.is_minimized() {
                window.restore();
            }
            window.show();
            window.activate();
        }
    }

    /// The CEF window itself, for a dialog that names it as its parent.
    pub(crate) fn into_cef(self) -> Window {
        self.cef
    }

    pub(crate) fn close(&self) {
        self.cef.close();
    }

    pub(crate) fn minimize(&self) {
        #[cfg(target_os = "windows")]
        {
            use windows_sys::Win32::UI::WindowsAndMessaging::*;
            let hwnd = self.cef.window_handle().0 as windows_sys::Win32::Foundation::HWND;
            if !hwnd.is_null() {
                unsafe {
                    PostMessageW(hwnd, WM_SYSCOMMAND, SC_MINIMIZE as usize, 0);
                }
            }
        }
        #[cfg(not(target_os = "windows"))]
        self.cef.minimize();
    }

    pub(crate) fn maximize(&self) {
        #[cfg(target_os = "windows")]
        {
            use windows_sys::Win32::UI::WindowsAndMessaging::*;
            let hwnd = self.cef.window_handle().0 as windows_sys::Win32::Foundation::HWND;
            if !hwnd.is_null() {
                unsafe {
                    PostMessageW(hwnd, WM_SYSCOMMAND, SC_MAXIMIZE as usize, 0);
                }
            }
        }
        #[cfg(not(target_os = "windows"))]
        self.cef.maximize();
    }

    pub(crate) fn restore(&self) {
        #[cfg(target_os = "windows")]
        {
            use windows_sys::Win32::UI::WindowsAndMessaging::*;
            let hwnd = self.cef.window_handle().0 as windows_sys::Win32::Foundation::HWND;
            if !hwnd.is_null() {
                unsafe {
                    PostMessageW(hwnd, WM_SYSCOMMAND, SC_RESTORE as usize, 0);
                }
            }
        }
        #[cfg(not(target_os = "windows"))]
        self.cef.restore();
    }

    pub(crate) fn is_maximized(&self) -> bool {
        self.cef.is_maximized() == 1
    }

    /// Reads the OS rather than a remembered state, `IsIconic` on Windows, so it
    /// answers for a window minimized by any route, including the raw
    /// `WM_SYSCOMMAND` this file posts.
    pub(crate) fn is_minimized(&self) -> bool {
        self.cef.is_minimized() == 1
    }

    pub(crate) fn show(&self) {
        self.cef.show();
    }

    pub(crate) fn hide(&self) {
        self.cef.hide();
    }

    /// Ask the platform to hand this window the foreground.
    ///
    /// On Windows this reaches the same `SetForegroundWindow` a hand-rolled call
    /// made, after a Z-order bump it did not do. On X11 it sends the EWMH
    /// `_NET_ACTIVE_WINDOW` message, which is the only thing a background app may
    /// use to ask, and which the hand-rolled path left undone entirely. On macOS
    /// `show` has already activated the app. CEF skips the work when the window
    /// holds the foreground already.
    pub(crate) fn activate(&self) {
        self.cef.activate();
    }

    pub(crate) fn client_area_bounds_in_screen(&self) -> Rect {
        self.cef.client_area_bounds_in_screen()
    }

    pub(crate) fn show_menu(
        &self,
        menu_model: Option<&mut MenuModel>,
        screen_point: Option<&Point>,
        anchor_position: MenuAnchorPosition,
    ) {
        self.cef
            .show_menu(menu_model, screen_point, anchor_position);
    }

    pub(crate) fn set_draggable_regions(&self, regions: Option<&[DraggableRegion]>) {
        self.cef.set_draggable_regions(regions);
    }

    pub(crate) fn set_title(&self, title: Option<&CefString>) {
        self.cef.set_title(title);
    }
}
