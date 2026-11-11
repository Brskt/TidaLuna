//! Tests for `src/ui/token_filter/request.rs`, attached to it by `#[path]`.

use super::*;

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
