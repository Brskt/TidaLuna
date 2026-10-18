//! Tests for `src/ui/csp_filter.rs`, attached to it by `#[path]`.

use cef::ResourceType;

use super::{is_document_url, strip_csp_meta};

fn url(s: &str) -> crate::ui::nav::RequestUrl {
    crate::ui::nav::RequestUrl::new(s.to_string())
}

/// `None` is the service-worker precache's view, with no browser and no type to
/// read; the path is all it has to go on.
#[test]
fn document_url_matches_shell_only() {
    assert!(is_document_url(&url("https://desktop.tidal.com/"), None));
    assert!(is_document_url(
        &url("https://desktop.tidal.com/index.html"),
        None
    ));
    assert!(is_document_url(
        &url("https://desktop.tidal.com/lastfmcallback.html"),
        None
    ));
    assert!(!is_document_url(
        &url("https://desktop.tidal.com/assets/index-abc.js"),
        None
    ));
    assert!(!is_document_url(
        &url("https://desktop.tidal.com/assets/x.css"),
        None
    ));
    assert!(!is_document_url(
        &url("https://resources.tidal.com/images/x/80x80.jpg"),
        None
    ));
    assert!(!is_document_url(
        &url("https://api.tidal.com/v1/tracks/1"),
        None
    ));
}

/// A cold start on a deep link asks for a route with no extension. The path rule
/// misses it, and only CEF's own word identifies the document.
#[test]
fn a_main_frame_load_is_a_document_whatever_its_path() {
    let deep = url("https://desktop.tidal.com/album/1");
    assert!(!is_document_url(&deep, None));
    assert!(is_document_url(&deep, Some(ResourceType::MAIN_FRAME)));
}

/// The login host navigates a top-level document of its own, in a popup. Stripping
/// the shell's CSP was never the deal there, and the type must not buy its way in.
#[test]
fn the_host_check_outranks_the_resource_type() {
    assert!(!is_document_url(
        &url("https://login.tidal.com/authorize"),
        Some(ResourceType::MAIN_FRAME)
    ));
}

#[test]
fn passthrough_when_absent() {
    let html = b"<html><head></head></html>";
    assert_eq!(strip_csp_meta(html), html);
}

#[test]
fn only_replaces_first() {
    let html = b"<meta http-equiv=\"Content-Security-Policy\" a><meta http-equiv=\"Content-Security-Policy\" b>";
    let out = strip_csp_meta(html);
    let s = std::str::from_utf8(&out).unwrap();
    assert_eq!(s.matches("LunaWuzHere").count(), 1);
    assert_eq!(s.matches("Content-Security-Policy").count(), 1);
}
