//! The one way a launch stops: say why where a desktop-launched user can see it,
//! then exit.
//!
//! A release build has no console: a log line alone leaves a Finder, Explorer
//! or menu launch with an app that simply does not open. Every module that can
//! end a launch (`main`, the macOS CEF bootstrap, the application singleton)
//! calls this instead of exiting on its own.

use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::ThreadId;

use super::process_args::is_helper_process;

/// Blocks on a dialog carrying `text`; `style` is the icon.
#[cfg(target_os = "windows")]
fn dialog(text: &str, style: u32) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{MB_OK, MessageBoxW};
    let text: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
    let title: Vec<u16> = "TidaLunar".encode_utf16().chain(Some(0)).collect();
    // SAFETY: both strings are NUL-terminated and outlive the call, and no
    // owner window is named.
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            text.as_ptr(),
            title.as_ptr(),
            MB_OK | style,
        )
    };
}

/// Blocks on an app-modal alert carrying `text`. It runs on the main thread and needs
/// neither CEF nor a prior `NSApplication`: `+sharedApplication` builds the singleton when
/// none exists and returns the existing one otherwise. The process exits right after; a
/// stock class built here cannot pre-empt the one `cef_app::install` registers.
#[cfg(target_os = "macos")]
fn alert(text: &str) {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSAlert, NSAlertStyle, NSApplication};
    use objc2_foundation::NSString;

    let Some(mtm) = MainThreadMarker::new() else {
        crate::verr!("[BOOT]   alert skipped: not on the main thread");
        return;
    };
    let _ = NSApplication::sharedApplication(mtm);
    let alert = NSAlert::new(mtm);
    alert.setAlertStyle(NSAlertStyle::Critical);
    alert.setMessageText(&NSString::from_str("TidaLunar could not start"));
    alert.setInformativeText(&NSString::from_str(text));
    alert.runModal();
}

/// Posts `text` as a critical notification that never expires, which the notification
/// server keeps after this process has exited. libdbus is already linked, and a native
/// dialog would need GTK, which packaging does not guarantee to be loadable here.
#[cfg(target_os = "linux")]
fn notify(text: &str) -> Result<(), String> {
    use dbus::arg::{RefArg, Variant};
    use std::collections::HashMap;
    use std::time::Duration;

    const SERVICE: &str = "org.freedesktop.Notifications";

    // Without an address libdbus tries to autolaunch a bus, which can start a daemon
    // or stall; a launch with no session has nobody to tell anyway.
    if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none() {
        return Err("no session bus".to_owned());
    }
    let conn = dbus::blocking::Connection::new_session().map_err(|e| e.to_string())?;
    let proxy = conn.with_proxy(
        SERVICE,
        "/org/freedesktop/Notifications",
        Duration::from_secs(2),
    );
    let (caps,): (Vec<String>,) = proxy
        .method_call(SERVICE, "GetCapabilities", ())
        .map_err(|e| e.to_string())?;
    // A server that reads body markup would take `<` or `&` in a path as tags;
    // one that does not would show the escapes literally.
    let body = if caps.iter().any(|cap| cap == "body-markup") {
        text.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    } else {
        text.to_owned()
    };
    let mut hints: HashMap<&str, Variant<Box<dyn RefArg>>> = HashMap::new();
    hints.insert("urgency", Variant(Box::new(2u8)));
    let _: (u32,) = proxy
        .method_call(
            SERVICE,
            "Notify",
            (
                "TidaLunar",
                0u32,
                "tidalunar",
                "TidaLunar could not start",
                body.as_str(),
                Vec::<&str>::new(),
                hints,
                0i32,
            ),
        )
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Logs and shows `why`, then exits 1.
///
/// A CEF subprocess only logs: it has no user to tell, and the browser that spawned it
/// reports its own absence. Callers need no role check of their own, which matters for the
/// ones that run before the role is known.
///
/// A test binary has nobody at the screen, and a modal alert there would hang the run instead
/// of failing it: the log line and the exit code carry the refusal.
pub(crate) fn refuse_to_start(why: &str) -> ! {
    crate::verr!("[BOOT]   {why}");
    if !cfg!(test) && !is_helper_process(std::env::args_os()) {
        #[cfg(target_os = "windows")]
        dialog(
            why,
            windows_sys::Win32::UI::WindowsAndMessaging::MB_ICONERROR,
        );
        #[cfg(target_os = "macos")]
        alert(why);
        #[cfg(target_os = "linux")]
        if let Err(e) = notify(why) {
            crate::verr!("[BOOT]   notification not delivered: {e}");
        }
    }
    std::process::exit(1);
}

/// The text a caught panic carried, if it carried one.
///
/// `panic!`, `assert!` and `unwrap` box a `&str` or a `String` depending on whether the message
/// was formatted (`Result::unwrap`'s is, `Option::unwrap`'s is not). Both shapes have to be
/// tried before giving up.
pub(crate) fn panic_message(payload: &(dyn std::any::Any + Send)) -> &str {
    payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| payload.downcast_ref::<&str>().copied())
        .unwrap_or("no message")
}

/// What the user is told when a panic ends the launch.
fn panic_refusal(message: &str, location: Option<(&str, u32)>) -> String {
    let at = location.map_or_else(String::new, |(file, line)| format!(" ({file}:{line})"));
    format!("TidaLunar hit an unexpected error while starting and cannot continue: {message}{at}")
}

/// Set once the browser and its window have been asked for; from then on a panic is the
/// ordinary kind again.
static LAUNCHED: AtomicBool = AtomicBool::new(false);

/// Makes a panic on the calling thread stop the launch through [`refuse_to_start`].
///
/// A panic otherwise ends the launch with a line on a stderr that a desktop launch does not
/// have: no log entry, no window, and an app that simply does not open. It is a hook rather than
/// a `catch_unwind` around `main` because the hook runs at the panic itself. Unwinding first
/// would drop the tokio runtime, which waits for every blocking task before returning, and
/// would never see a panic inside a CEF callback, which cannot unwind and aborts.
///
/// Call it from the browser process's main thread, which is where the launch runs, and not from
/// a CEF subprocess. Panics on other threads are left alone: a runtime task's panic is caught
/// and reported by its `JoinHandle`, and the launch does not depend on a worker. No code on the
/// main thread between here and [`launch_done`] may catch a panic, because the hook acts before
/// any `catch_unwind` could.
pub(crate) fn guard_launch() {
    install_launch_guard(std::thread::current().id(), &LAUNCHED, refuse_to_start);
}

/// Ends [`guard_launch`]'s watch: called when the browser and its window have been requested.
pub(crate) fn launch_done() {
    LAUNCHED.store(true, Ordering::Release);
}

/// [`guard_launch`] with the thread it watches, the flag that ends it and the refusal injected.
fn install_launch_guard(main: ThreadId, launched: &'static AtomicBool, refuse: fn(&str) -> !) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // The standard report first: it is the only one that carries a backtrace.
        previous(info);
        if launched.load(Ordering::Acquire) || std::thread::current().id() != main {
            return;
        }
        let location = info.location().map(|at| (at.file(), at.line()));
        refuse(&panic_refusal(panic_message(info.payload()), location));
    }));
}

#[cfg(test)]
#[path = "../../tests/unit/platform/refusal.rs"]
mod tests;
