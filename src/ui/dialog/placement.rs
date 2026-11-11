use cef::*;

/// DPI scale factor for a window (physical pixels / DIP). Returns 1.0 on failure.
#[cfg(target_os = "windows")]
unsafe fn dip_scale(hwnd: windows_sys::Win32::Foundation::HWND) -> f64 {
    use windows_sys::Win32::Graphics::Gdi::{GetDC, GetDeviceCaps, LOGPIXELSX, ReleaseDC};
    // SAFETY: hwnd is a valid window handle obtained from CEF's window_handle()
    let dc = unsafe { GetDC(hwnd) };
    let dpi = if !dc.is_null() {
        let d = unsafe { GetDeviceCaps(dc, LOGPIXELSX as i32) };
        unsafe { ReleaseDC(hwnd, dc) };
        d
    } else {
        96
    };
    if dpi > 0 { dpi as f64 / 96.0 } else { 1.0 }
}

/// Returns the main app window's HWND, or null.
#[cfg(target_os = "windows")]
fn get_main_hwnd() -> windows_sys::Win32::Foundation::HWND {
    crate::app_state::with_state(|s| s.browser.clone())
        .flatten()
        .and_then(|b| b.host())
        .map(|h| h.window_handle().0 as windows_sys::Win32::Foundation::HWND)
        .unwrap_or(std::ptr::null_mut())
}

/// Center the dialog on the main app window, clamped to the monitor work area.
/// Returns false if positioning failed (caller should fall back to center_window).
#[cfg(target_os = "windows")]
pub(super) fn center_on_parent(window: &mut Window, dialog_w: i32, dialog_h: i32) -> bool {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::GetWindowRect;

    let hwnd = get_main_hwnd();
    if hwnd.is_null() {
        return false;
    }

    // Get parent window rect in DIP
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    if unsafe { GetWindowRect(hwnd, &mut rect) } == 0 {
        return false;
    }
    let scale = unsafe { dip_scale(hwnd) };
    let parent_x = (rect.left as f64 / scale) as i32;
    let parent_y = (rect.top as f64 / scale) as i32;
    let parent_w = ((rect.right - rect.left) as f64 / scale) as i32;
    let parent_h = ((rect.bottom - rect.top) as f64 / scale) as i32;

    // Get monitor work area in DIP
    let hmonitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
    if hmonitor.is_null() {
        return false;
    }
    let mut mi = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        rcMonitor: RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        },
        rcWork: RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        },
        dwFlags: 0,
    };
    if unsafe { GetMonitorInfoW(hmonitor, &mut mi) } == 0 {
        return false;
    }
    let work_left = (mi.rcWork.left as f64 / scale) as i32;
    let work_top = (mi.rcWork.top as f64 / scale) as i32;
    let work_right = (mi.rcWork.right as f64 / scale) as i32;
    let work_bottom = (mi.rcWork.bottom as f64 / scale) as i32;

    // Center on parent, clamp within work area
    let x = (parent_x + (parent_w - dialog_w) / 2)
        .clamp(work_left, (work_right - dialog_w).max(work_left));
    let y = (parent_y + (parent_h - dialog_h) / 2)
        .clamp(work_top, (work_bottom - dialog_h).max(work_top));

    window.set_bounds(Some(&cef::Rect {
        x,
        y,
        width: dialog_w,
        height: dialog_h,
    }));
    true
}

#[cfg(not(target_os = "windows"))]
pub(super) fn center_on_parent(_window: &mut Window, _dialog_w: i32, _dialog_h: i32) -> bool {
    false
}

/// The main window, while it is on screen: a dialog it owns can no longer sink behind it. Never
/// modal, since a plugin reopening its box would lock out the settings that disable it. Hidden or
/// minimized, the main window would take the dialog out of sight.
pub(super) fn dialog_parent() -> Option<Window> {
    let window = crate::ui::app_window::AppWindow::current()?.into_cef();
    (window.is_visible() == 1 && window.is_minimized() == 0).then_some(window)
}
