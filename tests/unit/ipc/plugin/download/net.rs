//! Tests for `src/ipc/plugin/download/net.rs`, attached to it by `#[path]`.

use super::*;

/// A substring test accepts all three of these, and each is a host someone else can control.
#[test]
fn lookalike_hosts_are_refused() {
    assert!(!is_tidal_media_host("evil-audio.tidal.com"));
    assert!(!is_tidal_media_host("audio.tidal.com.evil.com"));
    assert!(!is_tidal_media_host("evil.com"));
    assert!(!is_tidal_media_host(""));
}

#[test]
fn artwork_comes_from_its_own_host_only() {
    assert!(is_tidal_artwork_host("resources.tidal.com"));
    assert!(!is_tidal_artwork_host("lgf.audio.tidal.com"));
    assert!(!is_tidal_artwork_host("resources.tidal.com.evil.com"));
}

/// Userinfo is the classic way past a check that reads the string rather than the parsed host (the
/// first url's host is `evil.com`), and plaintext would hand the stream to anyone on the path.
#[test]
fn only_an_https_url_whose_parsed_host_is_allowed_passes() {
    for (url, expected) in [
        ("https://lgf.audio.tidal.com@evil.com/track.flac", false),
        ("http://lgf.audio.tidal.com/track.flac", false),
        ("https://lgf.audio.tidal.com/track.flac", true),
        ("not a url", false),
        ("", false),
    ] {
        assert_eq!(allowed_url(url, is_tidal_media_host), expected, "{url:?}");
    }
}

/// Checking the submitted url is not enough on its own: the client follows redirects; an open
/// redirect on an allowed host would otherwise carry the fetch anywhere.
#[test]
fn a_redirect_leaving_the_allowed_hosts_is_refused() {
    let media = url::Url::parse("https://sp-ad-fa.audio.tidal.com/seg1.mp4").unwrap();
    let artwork = url::Url::parse("https://resources.tidal.com/cover.jpg").unwrap();
    let away = url::Url::parse("https://evil.com/seg1.mp4").unwrap();
    let plaintext = url::Url::parse("http://lgf.audio.tidal.com/track.flac").unwrap();

    assert!(redirect_allowed(&media));
    assert!(
        redirect_allowed(&artwork),
        "media and artwork share one client"
    );
    assert!(!redirect_allowed(&away));
    assert!(!redirect_allowed(&plaintext));
}
