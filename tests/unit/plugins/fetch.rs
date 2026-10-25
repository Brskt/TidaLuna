//! Tests for `src/plugins/fetch.rs`, attached to it by `#[path]`.

use super::*;

/// A true verdict attaches `state.captured_token`, the real access token, to the outgoing request.
/// This path is IPC straight into reqwest, never Blink; no mixed-content check upgrades or blocks
/// plaintext, which would put the live credential on the wire.
#[test]
fn the_real_token_goes_only_to_tidal_https_urls() {
    for (url, expected) in [
        ("https://api.tidal.com/v1/tracks/12345", true),
        ("https://api.tidalhifi.com/v1/tracks/12345", true),
        ("https://listen.tidal.com/v1/tracks", true),
        ("https://desktop.tidal.com/v1/tracks/123", true),
        (
            "https://openapi.tidal.com/v2/tracks?filter[isrc]=US1234",
            true,
        ),
        ("https://api.tidal.com:443/v1/tracks", true),
        ("https://example.com/api", false),
        ("https://evil.com/api.tidal.com", false),
        ("not-a-url", false),
        ("http://api.tidal.com/v1/tracks/12345", false),
        ("http://listen.tidal.com/v1/tracks", false),
        ("http://api.tidal.com:80/v1/tracks", false),
    ] {
        assert_eq!(is_tidal_api(url), expected, "{url:?}");
    }
}
