//! Tests for `src/video/cover/fetch.rs`, attached by `#[path]`.

use super::*;

/// A label-boundary match, never a substring (both lookalikes are registrable). The host comes
/// from the parsed url, plaintext is refused outright, and the static `images/` covers never pass.
#[test]
fn only_an_https_video_url_on_the_artwork_host_is_a_cover_url() {
    for (url, expected) in [
        (
            "https://resources.tidal.com/videos/a/b/c/1280x1280.mp4",
            true,
        ),
        (
            "https://resources.tidal.com.evil.example/videos/a.mp4",
            false,
        ),
        ("https://evil-resources.tidal.com/videos/a.mp4", false),
        (
            "https://resources.tidal.com@evil.example/videos/a.mp4",
            false,
        ),
        ("http://resources.tidal.com/videos/a.mp4", false),
        (
            "https://resources.tidal.com:8443/videos/a/640x640.mp4",
            false,
        ),
        (
            "https://user@resources.tidal.com/videos/a/640x640.mp4",
            false,
        ),
        (
            "https://:pass@resources.tidal.com/videos/a/640x640.mp4",
            false,
        ),
        ("not a url", false),
        (
            "https://resources.tidal.com/images/a/b/c/1280x1280.jpg",
            false,
        ),
    ] {
        assert_eq!(is_cover_url(url), expected, "{url}");
    }
}

const COVER_BASE: &str = "https://resources.tidal.com/videos/fc890b4a/ec2c/4729/9431/bcd88b1eedac";

/// The `src` is the page's word and the key names a file on disk: whatever the page varies (query,
/// fragment, variant, an edge between published sizes) lands on one key per cover and size,
/// rounded as the page's `servedEdge` rounds. Non-cover URLs are `is_cover_url`'s test.
#[test]
fn a_cover_key_names_one_published_size_and_refuses_what_is_not_a_cover() {
    let at = |edge: u32| format!("{COVER_BASE}/{edge}x{edge}.mp4");
    for (src, edge, expected) in [
        // TIDAL's album header asks for the 1280 file in a box it fills at 640.
        (at(1280), "640", Some(at(640))),
        (format!("{}?token=a#t=2", at(640)), "640", Some(at(640))),
        // Between published sizes: the next one up.
        (at(640), "100", Some(at(160))),
        // Past the largest: the largest.
        (at(640), "4096", Some(at(1280))),
        (format!("{COVER_BASE}/1280x720.mp4"), "640", None),
    ] {
        let got = CoverKey::parse(&src, edge)
            .ok()
            .map(|k| k.url().to_string());
        assert_eq!(got, expected, "{src} at {edge}");
    }
}
