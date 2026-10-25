//! Tests for `src/ui/token_filter.rs`, attached to it by `#[path]`.

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

#[test]
fn auto_injection_and_rewrite_share_the_api_host_set() {
    let u = |s: &str| RequestUrl::new(s.to_string());
    // The 5 API hosts must be recognised by both predicates (they delegate to
    // nav::is_tidal_api_host - pins the consolidation against drift).
    for url in [
        "https://api.tidal.com/v1/x",
        "https://api.tidalhifi.com/v1/x",
        "https://listen.tidal.com/x",
        "https://desktop.tidal.com/x",
        "https://openapi.tidal.com/x",
    ] {
        assert!(needs_auto_injection(&u(url)), "auto-inject: {url}");
        assert!(should_rewrite_token(&u(url)), "rewrite: {url}");
    }
    // auth/login are rewrite-only (not auto-injected).
    assert!(should_rewrite_token(&u(
        "https://auth.tidal.com/oauth2/token"
    )));
    assert!(!needs_auto_injection(&u(
        "https://auth.tidal.com/oauth2/token"
    )));
    // http scheme is rejected by both.
    assert!(!needs_auto_injection(&u("http://api.tidal.com/x")));
    assert!(!should_rewrite_token(&u("http://api.tidal.com/x")));
}

#[test]
fn token_response_drops_on_entropy_failure_never_passthrough() {
    // A real-token response with opaque generation failing must DROP
    // (Error), never Passthrough, or the real token reaches the renderer.
    let body = br#"{"access_token":"real-secret","refresh_token":"real-rt"}"#;
    assert!(matches!(
        process_token_response_with(body, None, 0, || None),
        ProcessResult::Error
    ));
}

/// A new refresh token binds the client id of its own exchange, never the stale prior: an
/// authorization_code after a client_credentials generation adopts "user", not "app". Without a
/// new refresh token, or with no client id observed, the prior is kept rather than blanked.
#[test]
fn a_generation_takes_the_exchange_client_id_only_with_a_new_refresh_token() {
    for (has_new_refresh_token, exchange, prior, expected) in [
        (true, Some("user"), Some("app"), "user"),
        (false, Some("app"), Some("user"), "user"),
        (true, None, Some("user"), "user"),
        (true, Some(""), Some("user"), "user"),
        (true, Some("user"), None, "user"),
        (false, Some("app"), None, "app"),
    ] {
        assert_eq!(
            resolve_generation_client_id(has_new_refresh_token, exchange, prior),
            expected,
            "new refresh: {has_new_refresh_token}, exchange: {exchange:?}, prior: {prior:?}"
        );
    }
}

fn generation(tag: &str) -> crate::platform::secure_store::TokenGeneration {
    crate::platform::secure_store::TokenGeneration {
        access_token: format!("real_at_{tag}"),
        refresh_token: format!("real_rt_{tag}"),
        opaque_at: format!("luna_at_{tag}"),
        opaque_rt: format!("luna_rt_{tag}"),
        version: 1,
        access_expires: u64::MAX,
        user_id: None,
        granted_scopes: vec![],
        client_id: "cid".into(),
    }
}

fn stored(
    current: crate::platform::secure_store::TokenGeneration,
    previous: Option<crate::platform::secure_store::TokenGeneration>,
    previous_valid_until: Option<u64>,
) -> crate::platform::secure_store::StoredTokenState {
    crate::platform::secure_store::StoredTokenState {
        current,
        previous,
        previous_valid_until,
    }
}

/// An opaque access token never leaves raw: past the window, or with no previous generation, it
/// resolves to the current real one.
#[test]
fn an_opaque_access_resolves_to_its_generation_within_the_window_else_the_current_one() {
    let windowed = stored(generation("new"), Some(generation("old")), Some(100));
    let alone = stored(generation("new"), None, None);
    for (state, opaque, now, expected) in [
        (&windowed, "luna_at_new", 50, "real_at_new"),
        (&windowed, "luna_at_old", 50, "real_at_old"),
        (&windowed, "luna_at_old", 200, "real_at_new"),
        (&alone, "luna_at_new", 0, "real_at_new"),
        (&alone, "luna_at_whatever", 0, "real_at_new"),
    ] {
        assert_eq!(
            resolve_opaque_access(state, opaque, now),
            expected,
            "{opaque} at {now}"
        );
    }
}

/// An opaque_rt the SDK held past our window must resolve to the current real refresh token,
/// never leave raw.
#[test]
fn an_opaque_refresh_resolves_to_its_generation_within_the_window_else_the_current_one() {
    let windowed = stored(generation("new"), Some(generation("old")), Some(100));
    let alone = stored(generation("new"), None, None);
    for (state, opaque, now, expected) in [
        (&windowed, "luna_rt_ancient", 50, "real_rt_new"),
        (&windowed, "luna_rt_old", 50, "real_rt_old"),
        (&windowed, "luna_rt_old", 200, "real_rt_new"),
        (&alone, "luna_rt_whatever", 0, "real_rt_new"),
    ] {
        assert_eq!(
            resolve_opaque_refresh(state, opaque, now),
            expected,
            "{opaque} at {now}"
        );
    }
}

/// The gate's rule, lifted off the handler so it can be exercised at all: the refusal branch ends
/// in an ungated `verr!`, and calling that from a test opens the real `console.log`.
#[test]
fn image_verdict_refuses_a_disallowed_image_and_admits_an_internal_surface_loading_itself() {
    for (what, resource_type, target, initiator, expected) in [
        (
            "an image on a foreign host",
            ResourceType::IMAGE,
            "https://evil.example/p.png",
            None,
            ImageVerdict::Refused,
        ),
        (
            "an image on an allowed host",
            ResourceType::IMAGE,
            "https://resources.tidal.com/p.png",
            None,
            ImageVerdict::Allowed,
        ),
        (
            "any other resource type passes untouched",
            ResourceType::MAIN_FRAME,
            "https://evil.example/p.png",
            None,
            ImageVerdict::NotAnImage,
        ),
        (
            "a plaintext image on an allowed host",
            ResourceType::IMAGE,
            "http://avatars.githubusercontent.com/u/1?v=4",
            None,
            ImageVerdict::Refused,
        ),
        (
            "an internal surface loading itself",
            ResourceType::IMAGE,
            "devtools://devtools/bundled/i.svg",
            Some(CefString::from("devtools://devtools")),
            ImageVerdict::Allowed,
        ),
        (
            "a foreign image beside an internal initiator",
            ResourceType::IMAGE,
            "https://evil.example/p.png",
            Some(CefString::from("devtools://devtools")),
            ImageVerdict::Refused,
        ),
        (
            "an internal url under a page initiator",
            ResourceType::IMAGE,
            "devtools://devtools/bundled/i.svg",
            Some(CefString::from("https://desktop.tidal.com")),
            ImageVerdict::Refused,
        ),
        (
            "an https initiator earns no self-load pass",
            ResourceType::IMAGE,
            "https://evil.example/a.png",
            Some(CefString::from("https://evil.example")),
            ImageVerdict::Refused,
        ),
    ] {
        let url = RequestUrl::new(target.to_string());
        assert_eq!(
            image_verdict(resource_type, &url, initiator.as_ref()),
            expected,
            "{what}"
        );
    }
}
