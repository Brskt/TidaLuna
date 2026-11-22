//! Tests for `src/plugins/outbound.rs`, attached to it by `#[path]`.

use super::*;
use serde_json::json;

const URL: &str = "https://example.com/p";

fn object(opts: Value) -> Result<OutboundRequest, Refusal> {
    OutboundRequest::from_object(URL, &opts.to_string())
}

fn pairs(req: Value) -> Result<OutboundRequest, Refusal> {
    OutboundRequest::from_pairs(&req)
}

/// No uppercasing, trimming or fallback to GET: `" GET"` and `"FOO"` are refused. Only the seven
/// verbs pass, in any ASCII case; Unicode case folding is not ASCII case.
#[test]
fn the_seven_verbs_in_any_case_and_nothing_else() {
    for (raw, expected) in [
        ("GET", Some(FetchMethod::Get)),
        ("get", Some(FetchMethod::Get)),
        ("post", Some(FetchMethod::Post)),
        ("PUT", Some(FetchMethod::Put)),
        ("pAtCh", Some(FetchMethod::Patch)),
        ("delete", Some(FetchMethod::Delete)),
        ("HEAD", Some(FetchMethod::Head)),
        ("options", Some(FetchMethod::Options)),
        (" GET", None),
        ("GET ", None),
        ("poſt", None),
        ("connect", None),
        ("TRACE", None),
        ("", None),
    ] {
        let admitted = object(json!({ "method": raw })).map(|r| r.method());
        match expected {
            Some(method) => assert_eq!(admitted, Ok(method), "{raw:?}"),
            None => assert!(
                matches!(admitted, Err(Refusal::Shape(ShapeError::Method(_)))),
                "{raw:?}: {admitted:?}"
            ),
        }
    }
    assert_eq!(object(json!({})).map(|r| r.method()), Ok(FetchMethod::Get));
    for missing in [json!({ "method": null }), json!({ "method": 7 })] {
        assert!(
            matches!(
                object(missing.clone()),
                Err(Refusal::Shape(ShapeError::Options(_)))
            ),
            "{missing}"
        );
    }
}

/// `fetch(url, { body: '' })` sends a body; a GET or HEAD with one is no request a server reads.
#[test]
fn a_body_on_get_or_head_is_refused_and_an_empty_one_counts_as_present() {
    for method in ["GET", "HEAD"] {
        for body in ["", "x"] {
            assert_eq!(
                object(json!({ "method": method, "body": body })).map(|r| r.method()),
                Err(Refusal::Shape(ShapeError::BodyOnGetOrHead)),
                "{method} {body:?}"
            );
        }
        assert!(object(json!({ "method": method, "body": null })).is_ok());
    }
    assert!(matches!(
        object(json!({ "method": "POST", "body": {} })),
        Err(Refusal::Shape(ShapeError::Options(_)))
    ));
}

/// A refused URL wins over any shape fault, a forged `Host` included, since it names a destination.
/// The status follows the class: 403 for where, 400 for what.
#[test]
fn the_url_is_judged_before_the_shape_and_each_refusal_has_its_status() {
    for (url, opts, expected, status) in [
        (
            "https://login.tidal.com/x",
            json!({ "method": "FOO" }),
            Refusal::Url(EgressRefusal::SignInHost),
            403,
        ),
        (
            "https://a..tidal.com/x",
            json!({ "method": "FOO" }),
            Refusal::Url(EgressRefusal::MalformedTidalHost),
            403,
        ),
        (
            URL,
            json!({ "method": "FOO", "headers": { "Host": "auth.tidal.com" } }),
            Refusal::Url(EgressRefusal::ForgedHostHeader),
            403,
        ),
        (
            "not a url",
            json!({}),
            Refusal::Url(EgressRefusal::Unparseable),
            403,
        ),
        (
            "file:///etc/passwd",
            json!({}),
            Refusal::Url(EgressRefusal::NotHttp),
            403,
        ),
        (
            URL,
            json!({ "method": "FOO" }),
            Refusal::Shape(ShapeError::Method("FOO".to_string())),
            400,
        ),
    ] {
        let refusal = OutboundRequest::from_object(url, &opts.to_string()).expect_err(url);
        assert_eq!(refusal, expected, "{url} {opts}");
        assert_eq!(refusal.status(), status, "{url} {opts}");
    }
}

/// `plugin.fetch` sends headers as an object, `net.fetch` as pairs: the same headers make the same
/// request, and neither lets a header name another host or reframe the body.
#[test]
fn the_same_headers_make_the_same_request_and_none_forges_the_host_or_the_framing() {
    // As a set: a JSON object keeps no order, the pairs keep theirs.
    let sorted = |r: OutboundRequest| {
        let mut h: Vec<(String, Vec<u8>)> = r
            .headers
            .into_iter()
            .map(|(n, v)| (n.as_str().to_string(), v.as_bytes().to_vec()))
            .collect();
        h.sort();
        h
    };
    let as_object = object(json!({ "method": "POST", "headers": { "X-A": "1", "Cookie": "a=b" } }));
    let as_pairs = pairs(json!({
        "url": URL,
        "method": "POST",
        "headers": [["X-A", "1"], ["Cookie", "a=b"]],
    }));
    assert_eq!(sorted(as_object.unwrap()), sorted(as_pairs.unwrap()));
    for (name, expected) in [
        ("host", Refusal::Url(EgressRefusal::ForgedHostHeader)),
        ("Content-Length", Refusal::Shape(ShapeError::FramingHeader)),
        (
            "transfer-encoding",
            Refusal::Shape(ShapeError::FramingHeader),
        ),
    ] {
        assert_eq!(
            object(json!({ "method": "POST", "headers": { name: "5" } })).map(|r| r.method()),
            Err(expected),
            "{name}"
        );
    }
}

/// A `net.fetch` body arrives in base64: over the cap or not base64, it is refused before it is
/// admitted, as is a header pair missing its value.
#[test]
fn a_net_fetch_body_is_capped_and_decoded_before_it_is_admitted() {
    let body = "A".repeat(MAX_BODY_BYTES / 3 * 4 + 8);
    assert_eq!(
        pairs(json!({ "url": URL, "method": "POST", "body": body })).map(|r| r.method()),
        Err(Refusal::Shape(ShapeError::Body("exceeds the limit")))
    );
    assert_eq!(
        pairs(json!({ "url": URL, "method": "POST", "body": "%%%" })).map(|r| r.method()),
        Err(Refusal::Shape(ShapeError::Body("is not base64")))
    );
    assert!(matches!(
        pairs(json!({ "url": URL, "headers": [["only-a-name"]] })),
        Err(Refusal::Shape(ShapeError::Options(_)))
    ));
}
