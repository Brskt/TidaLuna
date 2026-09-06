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

#[test]
fn new_refresh_token_binds_this_exchange_client_id_not_prior() {
    // The bug: authorization_code (new refresh_token, client_id "user")
    // arriving after a client_credentials gen (client_id "app") must adopt
    // "user", not stay stuck on the prior "app".
    assert_eq!(
        resolve_generation_client_id(true, Some("user"), Some("app")),
        "user"
    );
}

#[test]
fn no_new_refresh_token_keeps_prior_client_id() {
    // client_credentials (no refresh_token) must not overwrite the user
    // client_id already held.
    assert_eq!(
        resolve_generation_client_id(false, Some("app"), Some("user")),
        "user"
    );
}

#[test]
fn rotating_refresh_keeps_same_client_id() {
    // A refresh that rotates the token stays on the same client.
    assert_eq!(
        resolve_generation_client_id(true, Some("user"), Some("user")),
        "user"
    );
}

#[test]
fn empty_exchange_falls_back_to_prior_on_new_refresh() {
    // Defensive: a new refresh_token with no observed client_id keeps the
    // prior rather than blanking it.
    assert_eq!(
        resolve_generation_client_id(true, None, Some("user")),
        "user"
    );
    assert_eq!(
        resolve_generation_client_id(true, Some(""), Some("user")),
        "user"
    );
}

#[test]
fn first_gen_with_no_prior_takes_exchange() {
    // First persist after a session clear: no prior, take the exchange's.
    assert_eq!(
        resolve_generation_client_id(true, Some("user"), None),
        "user"
    );
    assert_eq!(
        resolve_generation_client_id(false, Some("app"), None),
        "app"
    );
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

#[test]
fn resolve_access_current_and_previous_within_window() {
    let ts = stored(generation("new"), Some(generation("old")), Some(100));
    // current opaque -> current real
    assert_eq!(resolve_opaque_access(&ts, "luna_at_new", 50), "real_at_new");
    // previous opaque, still in window -> previous real
    assert_eq!(resolve_opaque_access(&ts, "luna_at_old", 50), "real_at_old");
}

#[test]
fn resolve_access_out_of_window_previous_falls_back_to_current() {
    let ts = stored(generation("new"), Some(generation("old")), Some(100));
    // previous opaque, past the window -> current (not left raw)
    assert_eq!(
        resolve_opaque_access(&ts, "luna_at_old", 200),
        "real_at_new"
    );
}

#[test]
fn resolve_refresh_unknown_opaque_falls_back_to_current() {
    // The ~1h bug: an opaque_rt the SDK held past our window must resolve to
    // the current real refresh token, never leave raw.
    let ts = stored(generation("new"), Some(generation("old")), Some(100));
    assert_eq!(
        resolve_opaque_refresh(&ts, "luna_rt_ancient", 50),
        "real_rt_new"
    );
    // previous rt within window still maps to the matching real rt
    assert_eq!(
        resolve_opaque_refresh(&ts, "luna_rt_old", 50),
        "real_rt_old"
    );
    // and to current once out of window
    assert_eq!(
        resolve_opaque_refresh(&ts, "luna_rt_old", 200),
        "real_rt_new"
    );
}

#[test]
fn resolve_no_previous_maps_everything_to_current() {
    let ts = stored(generation("new"), None, None);
    assert_eq!(resolve_opaque_access(&ts, "luna_at_new", 0), "real_at_new");
    assert_eq!(
        resolve_opaque_access(&ts, "luna_at_whatever", 0),
        "real_at_new"
    );
    assert_eq!(
        resolve_opaque_refresh(&ts, "luna_rt_whatever", 0),
        "real_rt_new"
    );
}
