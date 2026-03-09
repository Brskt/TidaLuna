//! Windows taskbar thumbnail toolbar buttons (Previous / Play-Pause / Next).
//!
//! Uses the `ITaskbarList3` COM interface via a raw vtable wrapper — only the
//! four methods we need are typed, the rest are placeholder `usize` slots.
//! Icons are 16×16 BGRA bitmaps created at runtime via `CreateIconIndirect`.

use core::ffi::c_void;
use windows_sys::Win32::Foundation::{HWND, S_OK};
use windows_sys::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS,
    DeleteDC, DeleteObject,
};
use windows_sys::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows_sys::Win32::UI::Shell::THUMBBUTTON;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateIconIndirect, DestroyIcon, HICON, ICONINFO,
};

// ---- COM GUIDs ----

const CLSID_TASKBAR_LIST: windows_sys::core::GUID = windows_sys::core::GUID {
    data1: 0x56FDF344,
    data2: 0xFD6D,
    data3: 0x11d0,
    data4: [0x95, 0x8A, 0x00, 0x60, 0x97, 0xC9, 0xA0, 0x90],
};

const IID_ITASKBAR_LIST3: windows_sys::core::GUID = windows_sys::core::GUID {
    data1: 0xEA1AFB91,
    data2: 0x9E28,
    data3: 0x4B86,
    data4: [0x90, 0xE9, 0x9E, 0x9F, 0x8A, 0x5E, 0xEF, 0xAF],
};

// ---- Raw COM vtable for ITaskbarList3 ----

#[repr(C)]
struct ITaskbarList3Vtbl {
    // IUnknown (3)
    query_interface: usize,
    add_ref: usize,
    release: unsafe extern "system" fn(this: *mut c_void) -> u32,
    // ITaskbarList (5)
    hr_init: unsafe extern "system" fn(this: *mut c_void) -> i32,
    add_tab: usize,
    delete_tab: usize,
    activate_tab: usize,
    set_active_alt: usize,
    // ITaskbarList2 (1)
    mark_fullscreen_window: usize,
    // ITaskbarList3 (12)
    set_progress_value: usize,
    set_progress_state: usize,
    register_tab: usize,
    unregister_tab: usize,
    set_tab_order: usize,
    set_tab_active: usize,
    thumb_bar_add_buttons: unsafe extern "system" fn(
        this: *mut c_void,
        hwnd: HWND,
        count: u32,
        buttons: *const THUMBBUTTON,
    ) -> i32,
    thumb_bar_update_buttons: unsafe extern "system" fn(
        this: *mut c_void,
        hwnd: HWND,
        count: u32,
        buttons: *const THUMBBUTTON,
    ) -> i32,
    thumb_bar_set_image_list: usize,
    set_overlay_icon: usize,
    set_thumbnail_tooltip: usize,
    set_thumbnail_clip: usize,
}

// ---- Button IDs ----

const BTN_PREV: u32 = 0;
const BTN_PLAY_PAUSE: u32 = 1;
const BTN_NEXT: u32 = 2;

// ---- THUMBBUTTON flags / masks (windows-sys uses flat constants) ----

// THB_ICON = 0x2 — the hIcon field is valid
const THB_ICON: i32 = 0x2;
// THBF_ENABLED = 0x0
const THBF_ENABLED: i32 = 0x0;

// ---- 16×16 icon pixel data (BGRA, bottom-up) ----

/// Set a white pixel with a given alpha (premultiplied BGRA).
fn set_px(buf: &mut [u8; 16 * 16 * 4], row: u32, x: u32, alpha: u8) {
    let off = (row * 16 + x) as usize * 4;
    buf[off] = alpha; // B (premultiplied)
    buf[off + 1] = alpha; // G
    buf[off + 2] = alpha; // R
    buf[off + 3] = alpha; // A
}

/// Exact floating-point triangle width (9-row span, center at 8.0).
fn tri_exact(row: u32, max_w: f32) -> f32 {
    let dy = (row as f32 - 8.0).abs();
    ((4.5 - dy) / 4.5 * max_w).max(0.0)
}

/// Draw a right-pointing triangle with anti-aliased edges.
fn draw_tri_right(buf: &mut [u8; 16 * 16 * 4], x_base: u32, max_w: f32) {
    for row in 4u32..=12 {
        let w = tri_exact(row, max_w);
        if w <= 0.0 {
            continue;
        }
        let full = w.floor() as u32;
        let frac = w - w.floor();
        for x in x_base..x_base + full {
            set_px(buf, row, x, 0xFF);
        }
        if frac > 0.01 && (x_base + full) < 16 {
            set_px(buf, row, x_base + full, (frac * 255.0) as u8);
        }
    }
}

/// Draw a left-pointing triangle with anti-aliased edges.
fn draw_tri_left(buf: &mut [u8; 16 * 16 * 4], x_end: u32, max_w: f32) {
    for row in 4u32..=12 {
        let w = tri_exact(row, max_w);
        if w <= 0.0 {
            continue;
        }
        let full = w.floor() as u32;
        let frac = w - w.floor();
        let x_start = x_end + 1 - full;
        for x in x_start..=x_end {
            set_px(buf, row, x, 0xFF);
        }
        if frac > 0.01 && x_start > 0 {
            set_px(buf, row, x_start - 1, (frac * 255.0) as u8);
        }
    }
}

/// |◀  — thin bar + left-pointing triangle
fn draw_prev(buf: &mut [u8; 16 * 16 * 4]) {
    for row in 4u32..=12 {
        set_px(buf, row, 3, 0xFF);
        set_px(buf, row, 4, 0xFF);
    }
    draw_tri_left(buf, 12, 7.0);
}

/// ▶  — right-pointing triangle (play)
fn draw_play(buf: &mut [u8; 16 * 16 * 4]) {
    draw_tri_right(buf, 5, 7.0);
}

/// ⏸  — two vertical bars (pause)
fn draw_pause(buf: &mut [u8; 16 * 16 * 4]) {
    for row in 4u32..=12 {
        set_px(buf, row, 5, 0xFF);
        set_px(buf, row, 6, 0xFF);
        set_px(buf, row, 9, 0xFF);
        set_px(buf, row, 10, 0xFF);
    }
}

/// ▶|  — right-pointing triangle + thin bar
fn draw_next(buf: &mut [u8; 16 * 16 * 4]) {
    draw_tri_right(buf, 3, 7.0);
    for row in 4u32..=12 {
        set_px(buf, row, 11, 0xFF);
        set_px(buf, row, 12, 0xFF);
    }
}

/// Create a 16×16 HICON from a drawing function.
unsafe fn make_icon(draw: fn(&mut [u8; 16 * 16 * 4])) -> HICON {
    unsafe {
        let hdc = CreateCompatibleDC(core::ptr::null_mut());

        let mut bmi: BITMAPINFO = core::mem::zeroed();
        bmi.bmiHeader.biSize = core::mem::size_of::<BITMAPINFOHEADER>() as u32;
        bmi.bmiHeader.biWidth = 16;
        bmi.bmiHeader.biHeight = 16; // bottom-up
        bmi.bmiHeader.biPlanes = 1;
        bmi.bmiHeader.biBitCount = 32;
        bmi.bmiHeader.biCompression = BI_RGB;

        let mut bits: *mut c_void = core::ptr::null_mut();
        let color_bmp = CreateDIBSection(
            hdc,
            &bmi,
            DIB_RGB_COLORS,
            &mut bits,
            core::ptr::null_mut(),
            0,
        );
        if color_bmp.is_null() || bits.is_null() {
            DeleteDC(hdc);
            return core::ptr::null_mut();
        }

        // Fill pixel data
        let slice = core::slice::from_raw_parts_mut(bits as *mut u8, 16 * 16 * 4);
        let buf: &mut [u8; 16 * 16 * 4] = slice.try_into().expect("slice is 1024 bytes");
        buf.fill(0); // transparent
        draw(buf);

        // Create a monochrome mask (all zero = fully opaque when combined with alpha channel)
        let mut mask_bmi: BITMAPINFO = core::mem::zeroed();
        mask_bmi.bmiHeader.biSize = core::mem::size_of::<BITMAPINFOHEADER>() as u32;
        mask_bmi.bmiHeader.biWidth = 16;
        mask_bmi.bmiHeader.biHeight = 16;
        mask_bmi.bmiHeader.biPlanes = 1;
        mask_bmi.bmiHeader.biBitCount = 1;
        mask_bmi.bmiHeader.biCompression = BI_RGB;

        let mut mask_bits: *mut c_void = core::ptr::null_mut();
        let mask_bmp = CreateDIBSection(
            hdc,
            &mask_bmi,
            DIB_RGB_COLORS,
            &mut mask_bits,
            core::ptr::null_mut(),
            0,
        );
        if !mask_bmp.is_null() && !mask_bits.is_null() {
            // 16×16 at 1bpp = 4 bytes/row × 16 rows = 64 bytes. Fill with 0 (all opaque).
            let mask_slice = core::slice::from_raw_parts_mut(mask_bits as *mut u8, 4 * 16);
            mask_slice.fill(0);
        }

        let mut icon_info = ICONINFO {
            fIcon: 1, // TRUE = icon
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask_bmp,
            hbmColor: color_bmp,
        };

        let icon = CreateIconIndirect(&mut icon_info);

        DeleteObject(color_bmp);
        DeleteObject(mask_bmp);
        DeleteDC(hdc);

        icon
    }
}

// ---- ThumbBar ----

pub struct ThumbBar {
    taskbar: *mut c_void,
    vtbl: *const ITaskbarList3Vtbl,
    hwnd: HWND,
    icons: [HICON; 4], // prev, play, pause, next
}

impl ThumbBar {
    pub fn new(hwnd: HWND) -> Option<Self> {
        unsafe {
            let mut obj: *mut c_void = core::ptr::null_mut();
            let hr = CoCreateInstance(
                &CLSID_TASKBAR_LIST,
                core::ptr::null_mut(),
                CLSCTX_INPROC_SERVER,
                &IID_ITASKBAR_LIST3,
                &mut obj,
            );
            if hr != S_OK || obj.is_null() {
                crate::vprintln!("[THUMBBAR] CoCreateInstance failed: 0x{:08X}", hr);
                return None;
            }

            let vtbl = *(obj as *const *const ITaskbarList3Vtbl);
            let hr = ((*vtbl).hr_init)(obj);
            if hr != S_OK {
                crate::vprintln!("[THUMBBAR] HrInit failed: 0x{:08X}", hr);
                ((*vtbl).release)(obj);
                return None;
            }

            let icon_prev = make_icon(draw_prev);
            let icon_play = make_icon(draw_play);
            let icon_pause = make_icon(draw_pause);
            let icon_next = make_icon(draw_next);

            let buttons = make_buttons(icon_prev, icon_play, icon_next);
            let hr = ((*vtbl).thumb_bar_add_buttons)(obj, hwnd, 3, buttons.as_ptr());
            if hr != S_OK {
                crate::vprintln!("[THUMBBAR] ThumbBarAddButtons failed: 0x{:08X}", hr);
                // Still return Some — the COM object is valid, buttons may show later
            }

            Some(ThumbBar {
                taskbar: obj,
                vtbl,
                hwnd,
                icons: [icon_prev, icon_play, icon_pause, icon_next],
            })
        }
    }

    pub fn set_playing(&self, playing: bool) {
        let icon = if playing {
            self.icons[2] // pause icon
        } else {
            self.icons[1] // play icon
        };

        let buttons = make_buttons(self.icons[0], icon, self.icons[3]);
        unsafe {
            ((*self.vtbl).thumb_bar_update_buttons)(self.taskbar, self.hwnd, 3, buttons.as_ptr());
        }
    }
}

impl Drop for ThumbBar {
    fn drop(&mut self) {
        unsafe {
            ((*self.vtbl).release)(self.taskbar);
            for &icon in &self.icons {
                if !icon.is_null() {
                    DestroyIcon(icon);
                }
            }
        }
    }
}

fn make_buttons(prev_icon: HICON, center_icon: HICON, next_icon: HICON) -> [THUMBBUTTON; 3] {
    let mut buttons: [THUMBBUTTON; 3] = unsafe { core::mem::zeroed() };

    buttons[0].dwMask = THB_ICON;
    buttons[0].iId = BTN_PREV;
    buttons[0].hIcon = prev_icon;
    buttons[0].dwFlags = THBF_ENABLED;

    buttons[1].dwMask = THB_ICON;
    buttons[1].iId = BTN_PLAY_PAUSE;
    buttons[1].hIcon = center_icon;
    buttons[1].dwFlags = THBF_ENABLED;

    buttons[2].dwMask = THB_ICON;
    buttons[2].iId = BTN_NEXT;
    buttons[2].hIcon = next_icon;
    buttons[2].dwFlags = THBF_ENABLED;

    buttons
}
