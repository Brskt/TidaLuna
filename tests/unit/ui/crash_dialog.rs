//! Tests for `src/ui/crash_dialog.rs`, attached to it by `#[path]`.

use super::*;

#[test]
fn parse_crash_maps_the_three_schemes() {
    assert_eq!(parse_crash(CRASH_RELOAD), Some(CrashAction::Reload));
    assert_eq!(parse_crash(CRASH_OPEN), Some(CrashAction::OpenFolder));
    assert_eq!(parse_crash(CRASH_QUIT), Some(CrashAction::Quit));
    assert_eq!(parse_crash("https://desktop.tidal.com/"), None);
}

#[test]
fn build_html_wires_buttons_and_escapes() {
    let html = build_html("died <oops>", 42, None);
    assert!(html.contains("location.href='crash://reload'"));
    assert!(html.contains("location.href='crash://open'"));
    assert!(html.contains("location.href='crash://quit'"));
    assert!(html.contains("died &lt;oops&gt;"));
    assert!(html.contains("error code 42"));
}
