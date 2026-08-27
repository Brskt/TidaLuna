//! The `NSApplication` that Chromium requires of a macOS embedder.
//!
//! Chromium asks the application singleton, through a protocol of its own
//! making, whether AppKit is currently dispatching an event. It reads the answer
//! to decide when tearing an object down is safe: while an event is in flight, a
//! close is deferred to the top-level loop rather than run underneath the code
//! still using the object.
//!
//! A stock `NSApplication` has no answer to give. The question travels as a
//! plain Objective-C message, and the conformance check standing in front of it
//! is a `DCHECK`, compiled out of the release framework we ship. The message
//! lands on the singleton unprotected, and an unrecognised selector takes the
//! process down. Closing the window did exactly that.
//!
//! Only the browser process installs this. Upstream ships its subprocesses as a
//! separate executable whose entry point does not so much as include the header;
//! one binary here answers for every role, which puts the call past the point
//! where a subprocess has already exited.

use std::cell::Cell;

use cef::application_mac::{CefAppProtocol, CrAppControlProtocol, CrAppProtocol};
use objc2::rc::Retained;
use objc2::runtime::{Bool, NSObjectProtocol};
use objc2::{ClassType, DefinedClass, MainThreadMarker, define_class, extern_methods, msg_send};
use objc2_app_kit::{NSApp, NSApplication, NSEvent};

/// The one piece of state the protocol asks the application to hold.
///
/// Left unseeded deliberately. AppKit builds the singleton itself from inside
/// `+sharedApplication`, and a zeroed `Cell<Bool>` is already the `false` this
/// wants; objc2 only insists on an explicit `set_ivars` when the ivars type
/// needs dropping, which this one does not.
pub(crate) struct LunaApplicationIvars {
    /// True for as long as AppKit is dispatching an event through `sendEvent:`.
    handling_send_event: Cell<Bool>,
}

define_class!(
    /// The application singleton, carrying the conformance CEF asks for.
    #[unsafe(super(NSApplication))]
    #[ivars = LunaApplicationIvars]
    pub(crate) struct LunaApplication;

    impl LunaApplication {
        /// Raise the flag around AppKit's own event dispatch.
        ///
        /// Saved and restored rather than cleared: `sendEvent:` re-enters during
        /// nested dispatch, and clearing on the way out of an inner call would
        /// tell Chromium the outer event had finished while it is still running.
        /// This mirrors what CEF's own `CefScopedSendingEvent` does around the
        /// same call.
        #[unsafe(method(sendEvent:))]
        fn send_event(&self, event: &NSEvent) {
            let previous = self.ivars().handling_send_event.replace(Bool::YES);
            // SAFETY: the selector and its one argument match NSApplication's own
            // `sendEvent:`, which this overrides and whose implementation is what
            // super dispatches to.
            let _: () = unsafe { msg_send![super(self), sendEvent: event] };
            self.ivars().handling_send_event.set(previous);
        }
    }

    unsafe impl CrAppProtocol for LunaApplication {
        #[unsafe(method(isHandlingSendEvent))]
        fn is_handling_send_event(&self) -> Bool {
            self.ivars().handling_send_event.get()
        }
    }

    unsafe impl CrAppControlProtocol for LunaApplication {
        #[unsafe(method(setHandlingSendEvent:))]
        fn set_handling_send_event(&self, handling_send_event: Bool) {
            self.ivars().handling_send_event.set(handling_send_event);
        }
    }

    // Declared for the contract rather than the runtime. CEF's header only
    // forward-declares this one for an embedder to adopt, and it appears absent
    // from the shipped framework, in which case objc2 skips the registration
    // without complaint. Harmless either way: the selectors live on the two
    // protocols above, and each block registers its methods on its own.
    unsafe impl CefAppProtocol for LunaApplication {}
);

impl LunaApplication {
    extern_methods! {
        /// Sending `+sharedApplication` to *this* class is what makes AppKit
        /// build the singleton out of the subclass: the inherited implementation
        /// allocates `self`, and here `self` is `LunaApplication`. The binding
        /// generated on `NSApplication` cannot stand in for it, having the
        /// receiver hardcoded to `NSApplication` itself.
        #[unsafe(method(sharedApplication))]
        fn shared_application() -> Retained<Self>;
    }
}

/// Make this class the application singleton, or terminate the process.
///
/// Must run before `initialize()`: CEF creates the UI message pump inside it,
/// and the pump asks once, at creation, whether the singleton conforms. Answer
/// late and Chromium has already built a stock `NSApplication` of its own.
pub(crate) fn install() {
    let Some(mtm) = MainThreadMarker::new() else {
        crate::verr!("[CEF]    The application singleton has to be built on the main thread.");
        std::process::exit(1);
    };

    let _ = LunaApplication::shared_application();

    // Whoever asks for the singleton first keeps it for the life of the process,
    // and losing that race is silent. Left unchecked it would resurface later as
    // the very crash this module exists to prevent; it is verified here while
    // the cause is still legible.
    if !NSApp(mtm).isKindOfClass(LunaApplication::class()) {
        crate::verr!("[CEF]    Another NSApplication claimed the singleton first.");
        crate::verr!("[CEF]    Closing the window would abort; refusing to continue.");
        std::process::exit(1);
    }

    crate::vprintln!("[CEF]    Application singleton installed");
}

#[cfg(test)]
#[path = "../../tests/unit/platform/cef_app.rs"]
mod tests;
