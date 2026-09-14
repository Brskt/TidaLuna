//! Tests for `src/plugins/fetch.rs`, attached to it by `#[path]`.

use super::*;

#[test]
fn test_is_tidal_api() {
    assert!(is_tidal_api("https://api.tidal.com/v1/tracks/12345"));
    assert!(is_tidal_api("https://api.tidalhifi.com/v1/tracks/12345"));
    assert!(is_tidal_api("https://listen.tidal.com/v1/tracks"));
    assert!(is_tidal_api("https://desktop.tidal.com/v1/tracks/123"));
    assert!(is_tidal_api(
        "https://openapi.tidal.com/v2/tracks?filter[isrc]=US1234"
    ));
    assert!(is_tidal_api("https://api.tidal.com:443/v1/tracks"));
    assert!(!is_tidal_api("https://example.com/api"));
    assert!(!is_tidal_api("https://evil.com/api.tidal.com"));
    assert!(!is_tidal_api("not-a-url"));
}

#[test]
fn tidal_api_refuses_plaintext() {
    // A true verdict attaches `state.captured_token`, the real access token, to the outgoing
    // request. This path is IPC straight into reqwest, never Blink, so no mixed-content
    // check upgrades or blocks it: plaintext would put the live credential on the wire.
    assert!(!is_tidal_api("http://api.tidal.com/v1/tracks/12345"));
    assert!(!is_tidal_api("http://listen.tidal.com/v1/tracks"));
    assert!(!is_tidal_api("http://api.tidal.com:80/v1/tracks"));
}

#[test]
fn test_parse_fetch_opts() {
    let json = r#"{"method":"POST","headers":{"Content-Type":"application/json"},"body":"{\"key\":\"val\"}"}"#;
    let opts: FetchOpts = serde_json::from_str(json).unwrap();
    assert_eq!(opts.method, "POST");
    assert_eq!(
        opts.headers.get("Content-Type").unwrap().as_str().unwrap(),
        "application/json"
    );
    assert!(opts.body.is_some());
}

#[test]
fn test_parse_fetch_opts_defaults() {
    let json = r#"{}"#;
    let opts: FetchOpts = serde_json::from_str(json).unwrap();
    assert_eq!(opts.method, "GET");
    assert!(opts.headers.is_empty());
    assert!(opts.body.is_none());
}
