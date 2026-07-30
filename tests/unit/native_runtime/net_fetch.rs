//! Tests for `src/native_runtime/net_fetch.rs`, attached to it by `#[path]`.

use super::*;
use serde_json::json;

#[test]
fn parse_rejects_non_http_scheme() {
    // data:/blob: are resolved in the child; only http(s) reaches Rust.
    let req = json!({ "reqId": 1, "plugin": "P", "url": "file:///etc/passwd" });
    assert!(parse_fetch_request(&req).is_err());
}

#[test]
fn parse_rejects_missing_url() {
    let req = json!({ "reqId": 1, "plugin": "P" });
    assert!(parse_fetch_request(&req).is_err());
}

#[test]
fn parse_defaults_method_get() {
    let req = json!({ "reqId": 7, "plugin": "CoverTheme", "url": "https://h/p" });
    let p = parse_fetch_request(&req).expect("valid");
    assert_eq!(p.method, "GET");
    assert_eq!(p.plugin, "CoverTheme");
    assert!(p.body.is_none());
}

#[test]
fn parse_uppercases_method() {
    let req = json!({ "reqId": 1, "plugin": "P", "url": "https://h/p", "method": "post" });
    assert_eq!(parse_fetch_request(&req).expect("valid").method, "POST");
}

#[test]
fn parse_rejects_connect_and_trace_methods() {
    for m in ["CONNECT", "connect", "TRACE", "trace", "TRACK"] {
        let req = json!({ "reqId": 1, "plugin": "P", "url": "https://h/p", "method": m });
        assert!(
            parse_fetch_request(&req).is_err(),
            "method {m} must be rejected"
        );
    }
}

#[test]
fn parse_headers_array_preserves_duplicates_and_order() {
    let req = json!({
        "reqId": 1, "plugin": "P", "url": "https://h/p",
        "headers": [["accept", "a"], ["accept", "b"], ["x-test", "1"]]
    });
    let p = parse_fetch_request(&req).expect("valid");
    assert_eq!(
        p.headers,
        vec![
            ("accept".to_string(), "a".to_string()),
            ("accept".to_string(), "b".to_string()),
            ("x-test".to_string(), "1".to_string()),
        ]
    );
}

#[test]
fn parse_redirect_mode_defaults_follow_and_reads_values() {
    let base = json!({ "reqId": 1, "plugin": "P", "url": "https://h/p" });
    assert!(matches!(
        parse_fetch_request(&base).unwrap().redirect,
        RedirectMode::Follow
    ));
    let manual = json!({ "reqId": 1, "plugin": "P", "url": "https://h/p", "redirect": "manual" });
    assert!(matches!(
        parse_fetch_request(&manual).unwrap().redirect,
        RedirectMode::Manual
    ));
    let err = json!({ "reqId": 1, "plugin": "P", "url": "https://h/p", "redirect": "error" });
    assert!(matches!(
        parse_fetch_request(&err).unwrap().redirect,
        RedirectMode::Error
    ));
}

#[test]
fn parse_decodes_base64_body() {
    // base64("hello") = aGVsbG8=
    let req = json!({
        "reqId": 1, "plugin": "P", "url": "https://h/p",
        "method": "POST", "body": "aGVsbG8="
    });
    let p = parse_fetch_request(&req).expect("valid");
    assert_eq!(p.method, "POST");
    assert_eq!(p.body.as_deref(), Some(&b"hello"[..]));
}

#[test]
fn parse_rejects_bad_base64_body() {
    let req = json!({
        "reqId": 1, "plugin": "P", "url": "https://h/p", "body": "!!!not base64!!!"
    });
    assert!(parse_fetch_request(&req).is_err());
}

#[test]
fn parse_rejects_oversized_request_body_before_decoding() {
    // A base64 string whose decoded size would exceed the cap is rejected on
    // its length (cheap), before any large decode allocation.
    let oversized = "A".repeat(MAX_BODY_BYTES / 3 * 4 + 8);
    let req = json!({
        "reqId": 1, "plugin": "P", "url": "https://h/p", "method": "POST", "body": oversized
    });
    assert!(parse_fetch_request(&req).is_err());
}

#[test]
fn build_error_result_shape() {
    let line = build_error_result(&json!(9), "boom");
    let v: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(v["type"], "net.fetch.result");
    assert_eq!(v["reqId"], json!(9));
    assert_eq!(v["ok"], json!(false));
    assert_eq!(v["error"], "boom");
}

#[test]
fn build_ok_result_carries_url_redirected_and_dup_headers() {
    // Duplicate Set-Cookie must survive as an ordered array of [k,v] pairs.
    let headers = vec![
        ("set-cookie".to_string(), "a=1".to_string()),
        ("set-cookie".to_string(), "b=2".to_string()),
        ("content-type".to_string(), "image/jpeg".to_string()),
    ];
    let line = build_ok_result(
        &json!(3),
        200,
        "OK",
        &headers,
        "https://h/final",
        true,
        b"\x00\x01hi",
    );
    let v: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(v["ok"], json!(true));
    assert_eq!(v["status"], json!(200));
    assert_eq!(v["statusText"], "OK");
    assert_eq!(v["url"], "https://h/final");
    assert_eq!(v["redirected"], json!(true));
    let h = v["headers"].as_array().expect("headers array");
    assert_eq!(h.len(), 3);
    assert_eq!(h[0], json!(["set-cookie", "a=1"]));
    assert_eq!(h[1], json!(["set-cookie", "b=2"]));
    assert_eq!(h[2], json!(["content-type", "image/jpeg"]));
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(v["body"].as_str().unwrap())
        .unwrap();
    assert_eq!(decoded, b"\x00\x01hi");
}
