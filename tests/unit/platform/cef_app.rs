//! Tests for `src/platform/cef_app.rs`, attached to it by `#[path]`.
//!
//! macOS-only, like the module itself, and they stop at the class on purpose:
//! the harness runs tests off the main thread, where `MainThreadMarker::new()`
//! yields nothing and no `NSApplication` instance can exist. What remains
//! within reach is the half that was missing - whether the selectors Chromium
//! sends are defined on our class at all.

use super::*;
use objc2::runtime::Sel;
use objc2::sel;

/// True when the class itself defines the selector, rather than inheriting it.
///
/// `instance_method` answers for the whole chain, which cannot tell an override
/// from `NSApplication`'s own implementation; this list holds only what was
/// registered on this class.
fn defines(selector: Sel) -> bool {
    LunaApplication::class()
        .instance_methods()
        .iter()
        .any(|method| method.name() == selector)
}

#[test]
fn the_class_extends_nsapplication() {
    assert_eq!(
        LunaApplication::class().superclass(),
        Some(NSApplication::class())
    );
}

#[test]
fn the_selector_that_took_the_process_down_is_answered() {
    // The crash read `-[NSApplication isHandlingSendEvent]: unrecognized
    // selector`, sent with no guard in front of it by a release framework. This
    // is the assertion that failed for every build before this class existed.
    assert!(defines(sel!(isHandlingSendEvent)));
}

#[test]
fn the_flag_can_be_written_as_well_as_read() {
    // Chromium's scoper reads the flag, forces it, then restores it on the way
    // out. Answering only the read would move the same crash to the restore.
    assert!(defines(sel!(setHandlingSendEvent:)));
}

#[test]
fn appkits_dispatch_is_intercepted_rather_than_inherited() {
    // The flag means nothing unless something raises it, and AppKit's own event
    // dispatch is the only place that can.
    assert!(defines(sel!(sendEvent:)));
}
