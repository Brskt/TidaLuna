//! Console events as quit requests (Windows): Ctrl+C, Ctrl+Break, closing the console.
//!
//! Chromium registers a handler during `initialize` that ends the process from a posted task,
//! without `main` returning, and never hands the event on. Registered after it, ours runs first.

use std::time::Duration;

use windows_sys::Win32::System::Console::{
    CTRL_BREAK_EVENT, CTRL_C_EVENT, CTRL_CLOSE_EVENT, SetConsoleCtrlHandler,
};
use windows_sys::core::BOOL;

/// Windows ends the process 5 s after its console closes; this plus the bounded flush fits.
const CLOSE_GRACE: Duration = Duration::from_secs(3);

/// Called once `initialize` has returned, after Chromium's own registration.
pub(crate) fn install() {
    // SAFETY: the handler is a plain function that lives as long as the process.
    if unsafe { SetConsoleCtrlHandler(Some(on_event), 1) } == 0 {
        let e = std::io::Error::last_os_error();
        crate::verr!("[QUIT]   Console events keep Chromium's handler ({e})");
    }
}

/// Runs on a thread Windows creates for the event.
unsafe extern "system" fn on_event(event: u32) -> BOOL {
    match event {
        CTRL_C_EVENT | CTRL_BREAK_EVENT => {
            super::request(super::End::ConsoleInterrupt, super::GRACE);
            1
        }
        CTRL_CLOSE_EVENT => {
            super::request(super::End::ConsoleClose, CLOSE_GRACE);
            // Returning would let Windows end the process at once. Wait for `main` or the watchdog.
            loop {
                std::thread::park();
            }
        }
        // Logoff and shutdown reach a process with windows as `WM_ENDSESSION` instead.
        _ => 0,
    }
}
