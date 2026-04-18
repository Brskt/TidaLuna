//! Minimal Xlib FFI to set `WM_CLASS` on a CEF Views top-level window.
//!
//! CEF Views builds X11 windows itself and never threads Chromium's
//! `--class=NAME` switch through, so the window starts with no class hint and
//! GNOME (which only matches `.desktop` entries by `WM_CLASS`) falls back to a
//! generic dock icon. We open a private X connection here, push an
//! `XClassHint`, then close it - the window manager picks it up immediately.
#![cfg(target_os = "linux")]

use std::ffi::{CString, c_void};
use std::os::raw::{c_char, c_int, c_ulong};

#[repr(C)]
struct XClassHint {
    res_name: *mut c_char,
    res_class: *mut c_char,
}

#[link(name = "X11")]
unsafe extern "C" {
    fn XOpenDisplay(name: *const c_char) -> *mut c_void;
    fn XCloseDisplay(display: *mut c_void) -> c_int;
    fn XSetClassHint(display: *mut c_void, w: c_ulong, hints: *mut XClassHint) -> c_int;
    fn XFlush(display: *mut c_void) -> c_int;
}

pub(crate) fn set_wm_class(xid: c_ulong, class: &str) {
    let Ok(c) = CString::new(class) else {
        return;
    };
    unsafe {
        let display = XOpenDisplay(std::ptr::null());
        if display.is_null() {
            crate::vprintln!("[X11]    XOpenDisplay failed; WM_CLASS not set");
            return;
        }
        let mut hint = XClassHint {
            res_name: c.as_ptr() as *mut c_char,
            res_class: c.as_ptr() as *mut c_char,
        };
        XSetClassHint(display, xid, &mut hint);
        XFlush(display);
        XCloseDisplay(display);
    }
}
