//! Tests for `src/ui/csp_filter.rs`, attached to it by `#[path]`.

use super::{is_document_url, strip_csp_meta};

#[test]
fn document_url_matches_shell_only() {
    let u = |s: &str| crate::ui::nav::RequestUrl::new(s.to_string());
    assert!(is_document_url(&u("https://desktop.tidal.com/")));
    assert!(is_document_url(&u("https://desktop.tidal.com/index.html")));
    assert!(is_document_url(&u(
        "https://desktop.tidal.com/lastfmcallback.html"
    )));
    assert!(!is_document_url(&u(
        "https://desktop.tidal.com/assets/index-abc.js"
    )));
    assert!(!is_document_url(&u(
        "https://desktop.tidal.com/assets/x.css"
    )));
    assert!(!is_document_url(&u(
        "https://resources.tidal.com/images/x/80x80.jpg"
    )));
    assert!(!is_document_url(&u("https://api.tidal.com/v1/tracks/1")));
}

#[test]
fn strips_csp_meta_tag() {
    let html =
        b"<html><head><meta http-equiv=\"Content-Security-Policy\" content=\"x\"></head></html>";
    let out = strip_csp_meta(html);
    let s = std::str::from_utf8(&out).unwrap();
    assert!(s.contains("<meta name=\"LunaWuzHere\""));
    assert!(!s.contains("Content-Security-Policy"));
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
