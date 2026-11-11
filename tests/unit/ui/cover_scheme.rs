//! Tests for `src/ui/cover_scheme/mod.rs`, attached by `#[path]`.

use super::*;

/// Every other requester is refused before work starts, a service worker's own fetch (which
/// arrives with no frame) included.
#[test]
fn only_a_tidal_page_may_request_a_cover() {
    for (frame, refused) in [
        (Some("https://desktop.tidal.com/"), false),
        (Some("https://desktop.tidal.com/album/1"), false),
        (Some("https://desktop.tidal.com/login"), false),
        (Some("https://desktop.tidal.com/login/auth"), false),
        (Some(""), true),
        (Some("about:blank"), true),
        (Some("data:text/html;base64,PGgxPmhpPC9oMT4="), true),
        (Some("devtools://devtools/bundled/inspector.html"), true),
        (Some("https://evil.example/"), true),
        (Some("https://desktop.tidal.com.evil.example/"), true),
        (Some("https://listen.tidal.com/"), true),
        (None, true),
    ] {
        assert_eq!(requester_refusal(frame).is_some(), refused, "{frame:?}");
    }
}

fn key(name: &str) -> CoverKey {
    CoverKey::parse(
        &format!("https://resources.tidal.com/videos/{name}/640x640.mp4"),
        "640",
    )
    .unwrap()
}

fn grant(at: Instant) -> Grant {
    Grant {
        width: 640,
        height: 640,
        frames: 301,
        span_micros: 10_000_000,
        at,
    }
}

/// A WebM is checked against what a decode measured, never against what the page says: one pass
/// grants one store, never a replay that would evict real covers.
#[test]
fn a_store_is_granted_once_per_finished_pass() {
    let now = Instant::now();
    let mut grants = StoreGrants::default();
    grants.insert(key("a"), grant(now));
    assert!(grants.take(&key("a"), now).is_some());
    assert!(
        grants.take(&key("a"), now).is_none(),
        "the grant is used up"
    );
}
