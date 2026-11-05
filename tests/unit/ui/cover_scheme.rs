//! Tests for `src/ui/cover_scheme.rs`, attached by `#[path]`.

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

/// The sizes are the page's; their sum sets the allocation, capped before anything is copied.
#[test]
fn an_upload_over_the_cap_is_refused_before_it_is_allocated() {
    for (sizes, max, expected) in [(vec![3, 4], 8, Ok(7)), (vec![5, 4], 8, Err(413))] {
        assert_eq!(
            body_size(sizes.clone(), max).map_err(|(status, _)| status),
            expected,
            "{sizes:?} under {max}"
        );
    }
}

/// Chromium places a `bytes=N-` body at N on our scheme without checking a status or a
/// `Content-Range`: CEF's skip to N is acknowledged only over a body that starts there.
#[test]
fn a_range_skip_is_acknowledged_only_over_the_part_that_starts_there() {
    let cover: Vec<u8> = (0..=255).collect();
    let part = ranged_answer(cover.clone(), Some("bytes=100-"));
    assert_eq!(part.body, cover[100..]);
    assert_eq!(part.skip(100), SkipVerdict::Skipped(100));
    assert_eq!(part.skip(99), SkipVerdict::Refused, "another offset");

    for (answer, case) in [
        (ranged_answer(cover.clone(), None), "the whole cover"),
        (ranged_answer(cover, Some("bytes=300-")), "past the end"),
    ] {
        assert_eq!(answer.skip(100), SkipVerdict::Refused, "{case}");
    }
}

/// The page decides by the status alone: 502 asks again soon, 503 waits for the codec, 507
/// asks at the next miss, and anything else spends one of two attempts. A transient answered as
/// spent freezes a playable cover; a fault answered as transient retries it without end.
#[test]
fn each_failure_reaches_the_page_as_the_status_that_says_what_to_do() {
    for (class, status) in [
        (FailureClass::Network, 502),
        (FailureClass::Refused, 422),
        (FailureClass::Corrupt, 422),
        (FailureClass::Unloadable, 503),
        (FailureClass::Disk, 500),
    ] {
        assert_eq!(frames_refusal(class).0, status, "{class:?}");
    }
    for (kind, status) in [
        (std::io::ErrorKind::StorageFull, 507),
        (std::io::ErrorKind::PermissionDenied, 500),
    ] {
        assert_eq!(put_refusal(&kind.into()).0, status, "{kind:?}");
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
