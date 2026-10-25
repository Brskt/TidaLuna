//! Tests for `src/native_runtime/net_fetch.rs`, attached to it by `#[path]`.

use super::*;
use serde_json::json;

/// `data:`/`blob:` are resolved in the child; only http(s) reaches Rust. Also refused: CONNECT,
/// TRACE and TRACK in any case, and an oversized body (on its base64 length, before any decode).
#[test]
fn a_fetch_request_outside_the_allowed_shape_is_rejected() {
    let with_method =
        |m: &str| json!({ "reqId": 1, "plugin": "P", "url": "https://h/p", "method": m });
    let oversized = "A".repeat(MAX_BODY_BYTES / 3 * 4 + 8);

    for (what, req) in [
        (
            "a file: url",
            json!({ "reqId": 1, "plugin": "P", "url": "file:///etc/passwd" }),
        ),
        ("method CONNECT", with_method("CONNECT")),
        ("method connect", with_method("connect")),
        ("method TRACE", with_method("TRACE")),
        ("method trace", with_method("trace")),
        ("method TRACK", with_method("TRACK")),
        (
            "a body over the cap",
            json!({
                "reqId": 1, "plugin": "P", "url": "https://h/p", "method": "POST", "body": oversized
            }),
        ),
    ] {
        assert!(
            parse_fetch_request(&req).is_err(),
            "{what} must be rejected"
        );
    }
}

/// The jar is keyed by module name, which a reinstall reuses: forgetting it is what keeps a new
/// install from sending the old one's cookies.
#[test]
fn forgetting_a_module_drops_its_cookie_jar() {
    let module = "ForgetJar/x.native.ts";
    let _ = client_for(module, RedirectMode::Follow);
    let has = || PLUGIN_CLIENTS.lock().unwrap().contains_key(module);
    assert!(has());
    forget_clients(module);
    assert!(!has());
}
