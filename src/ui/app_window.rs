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

    pub(crate) fn show(&self) {
        self.cef.show();
    }

    pub(crate) fn focus_foreground(&self) {
        #[cfg(target_os = "windows")]
        {
            let hwnd = self.cef.window_handle().0 as windows_sys::Win32::Foundation::HWND;
            if !hwnd.is_null() {
                unsafe {
                    windows_sys::Win32::UI::WindowsAndMessaging::SetForegroundWindow(hwnd);
                }
            }
        }
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
