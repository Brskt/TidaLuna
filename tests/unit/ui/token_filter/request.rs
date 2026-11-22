//! Tests for `src/ui/token_filter/request.rs`, attached to it by `#[path]`.

use super::*;
use crate::platform::secure_store::ClientBinding;

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
        client: ClientBinding::Proven("cid".into()),
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
            resolve_opaque_refresh(state, opaque, now).refresh_token,
            expected,
            "{opaque} at {now}"
        );
    }
}

const NONCE: &str = "luna_0123456789abcdef0123456789abcdef";

/// A bearer nonce resolves to the session's access token, or the header goes: the placeholder
/// never leaves, nor does a nonce's shape under any other scheme.
#[test]
fn a_bearer_nonce_is_resolved_or_the_header_is_stripped() {
    let live = stored(generation("new"), None, None);
    let in_basic = format!("Basic {NONCE}");
    for (header, with_session, without, why) in [
        (
            "Bearer luna_at_new",
            BearerFate::Substitute("Bearer real_at_new".into()),
            BearerFate::Strip,
            "a bearer nonce",
        ),
        (
            "Bearer real_token",
            BearerFate::Leave,
            BearerFate::Leave,
            "a real token",
        ),
        (
            in_basic.as_str(),
            BearerFate::Strip,
            BearerFate::Strip,
            "a nonce under another scheme",
        ),
    ] {
        let session = HeldTokens {
            user: Some(&live),
            app: None,
            user_live: true,
        };
        let nothing = HeldTokens {
            user: None,
            app: None,
            user_live: false,
        };
        assert_eq!(
            BearerFate::decide(header, &session, 0),
            with_session,
            "{why}"
        );
        assert_eq!(BearerFate::decide(header, &nothing, 0), without, "{why}");
    }
}

/// Which token an access nonce stands for. A nonce one of them minted names it exactly; an
/// unknown one means the live user's current token, else the app token, else nothing, and the
/// header then goes rather than the placeholder.
#[test]
fn an_access_nonce_resolves_to_the_token_that_minted_it_else_by_who_is_live() {
    let user = stored(generation("new"), Some(generation("old")), Some(100));
    let app = AppToken {
        opaque_at: "luna_at_app".into(),
        access_token: "real_at_app".into(),
    };
    let target = |user_state, app_state, user_live, nonce: &str| {
        let held = HeldTokens {
            user: user_state,
            app: app_state,
            user_live,
        };
        match NonceTarget::of(&held, nonce, 50) {
            NonceTarget::User(generation) => generation.access_token.clone(),
            NonceTarget::Application(app) => app.access_token.clone(),
            NonceTarget::Unresolvable => "stripped".into(),
        }
    };
    for (user_state, app_state, user_live, nonce, expected, why) in [
        (
            Some(&user),
            Some(&app),
            true,
            "luna_at_new",
            "real_at_new",
            "the current generation's nonce",
        ),
        (
            Some(&user),
            Some(&app),
            true,
            "luna_at_old",
            "real_at_old",
            "the previous one, in its window",
        ),
        (
            Some(&user),
            Some(&app),
            true,
            "luna_at_app",
            "real_at_app",
            "the app token's nonce",
        ),
        (
            Some(&user),
            Some(&app),
            true,
            "luna_at_unknown",
            "real_at_new",
            "unknown, a live user",
        ),
        (
            Some(&user),
            Some(&app),
            false,
            "luna_at_new",
            "real_at_new",
            "a held session's own nonce",
        ),
        (
            Some(&user),
            Some(&app),
            false,
            "luna_at_unknown",
            "real_at_app",
            "unknown, the user held",
        ),
        (
            Some(&user),
            None,
            false,
            "luna_at_unknown",
            "stripped",
            "unknown, held, no app token",
        ),
    ] {
        assert_eq!(
            target(user_state, app_state, user_live, nonce),
            expected,
            "{why}"
        );
    }
}

/// Only TIDAL's own hydrate POST is the surface held back; nothing near it is.
#[test]
fn the_hydrate_surface_is_the_exact_post_and_nothing_near_it() {
    let u = |s: &str| RequestUrl::new(s.to_string());
    for (method, url, expected) in [
        ("POST", "https://tidal.com/api/session/hydrate", true),
        ("POST", "https://tidal.com/api/session/hydrate?x=1", true),
        ("GET", "https://tidal.com/api/session/hydrate", false),
        ("POST", "http://tidal.com/api/session/hydrate", false),
        (
            "POST",
            "https://desktop.tidal.com/api/session/hydrate",
            false,
        ),
        ("POST", "https://tidal.com./api/session/hydrate", false),
        ("POST", "https://tidal.com/api/session/hydrate/", false),
    ] {
        assert_eq!(
            BodyTokenSurface::of(method, &u(url)).is_some(),
            expected,
            "{method} {url}"
        );
    }
}
