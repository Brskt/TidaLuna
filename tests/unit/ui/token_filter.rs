//! Tests for `src/ui/token_filter/mod.rs`, attached to it by `#[path]`.

use super::*;

#[test]
fn opaque_is_none_on_entropy_failure() {
    assert_eq!(generate_opaque_with(|_| false), None);
}

#[test]
fn opaque_has_prefix_and_hex_bytes() {
    let o = generate_opaque_with(|buf| {
        buf.fill(0xab);
        true
    })
    .expect("RNG ok");
    assert!(o.starts_with(OPAQUE_PREFIX));
    let hex = &o[OPAQUE_PREFIX.len()..];
    assert_eq!(hex.len(), 32);
    assert_eq!(hex, "ab".repeat(16));
}

/// A nonce's shape is the prefix and the lowercase hex of its bytes, anywhere in the text: anything
/// shorter or cased otherwise is not one.
#[test]
fn the_detector_finds_a_nonce_shape_and_nothing_shorter_or_cased_differently() {
    let hex = "0123456789abcdef".repeat(2);
    for (text, expected) in [
        (format!("luna_{}", &hex[..31]), false),
        (format!("luna_{}", hex.to_uppercase()), false),
        (format!("luna_luna_{hex}"), true),
        ("luna_rt_new".to_string(), false),
    ] {
        assert_eq!(carries_nonce_shape(&text), expected, "{text}");
    }
}

/// A redirect keeps the origin only by asking the token endpoint the same thing again; leaving it
/// or asking something else makes the answer inert, and nothing raises it back.
#[test]
fn an_exchange_origin_only_ever_lowers_across_hops() {
    use ExchangeOrigin::{Code, Inert, Substituted, WebSession};
    for (prev, hop, expected) in [
        (None, None, None),
        (None, Some(Substituted), Some(Substituted)),
        (Some(Code), Some(Code), Some(Code)),
        (Some(WebSession), Some(Code), Some(Inert)),
        (Some(Substituted), None, Some(Inert)),
        (Some(Inert), Some(Substituted), Some(Inert)),
    ] {
        assert_eq!(
            ExchangeOrigin::carry(prev, hop),
            expected,
            "{prev:?} then {hop:?}"
        );
    }
}

/// An answer commits only as what its request could produce: a forged query cannot turn a
/// refresh's answer into an app token, a refresh into a sign-in, nor an inert request into anything.
#[test]
fn an_answer_commits_only_as_what_its_request_could_produce() {
    use crate::app_state::{GrantKind::*, UserCommit};
    use Answer::{Application as App, Drop, SameUserInPlace, User};
    use ExchangeOrigin::{ClientCredentials, Code, Inert, Substituted, WebSession};
    for (origin, expected) in [
        (
            Substituted,
            [User(UserCommit::Refresh), User(UserCommit::Refresh), Drop],
        ),
        (Code, [User(UserCommit::Login), Drop, Drop]),
        (ClientCredentials, [Drop, Drop, App]),
        (WebSession, [SameUserInPlace, Drop, Drop]),
        (Inert, [Drop, Drop, Drop]),
    ] {
        for (kind, expected) in [Login, Refresh, Application].into_iter().zip(expected) {
            assert_eq!(
                origin.answer(kind),
                expected,
                "{origin:?} answered {kind:?}"
            );
        }
    }
}

#[test]
fn rewrite_covers_an_api_host_and_the_auth_host() {
    let u = |s: &str| RequestUrl::new(s.to_string());
    assert!(should_rewrite_token(&u("https://api.tidal.com/v1/x")));
    assert!(should_rewrite_token(&u(
        "https://auth.tidal.com/oauth2/token"
    )));
}

/// The telemetry hosts carry the token too, the region-less one included. An empty label is no
/// region: a suffix test let that host take the real token.
#[test]
fn the_event_collector_is_a_tidalhi_name_whatever_its_region() {
    let u = |s: &str| RequestUrl::new(s.to_string());
    for (url, expected) in [
        ("https://event-collector.eu.tidalhi.fi/x", true),
        ("https://event-collector.tidalhi.fi/x", true),
        ("https://event-collector..tidalhi.fi/x", false),
        ("https://event-collector.eu.tidalhi.fi.evil.com/x", false),
        ("https://evil-event-collector.eu.tidalhi.fi/x", false),
        ("http://event-collector.eu.tidalhi.fi/x", false),
    ] {
        assert_eq!(should_rewrite_token(&u(url)), expected, "{url}");
    }
}
