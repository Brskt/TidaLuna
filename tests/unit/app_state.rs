//! Tests for `src/app_state.rs`, attached to it by `#[path]`.

use std::sync::Arc;

use super::{
    AppState, MeasuredDuration, PagePhase, PassStood, PluginCredential, Ticket, TokenCommit,
    TokenGrant, UserCommit, end_page_music, js_ipc_response, js_string_literal,
    settle_recorded_duration,
};
use crate::bridge::PlayerBridgeEvent;
use crate::platform::secure_store::{ClientBinding, StoredTokenState, TokenGeneration};
use crate::player::{LoadOrigin, Player, begin_load, current_load};
use crate::util::now_unix_secs;

/// A measurement as an ingress hands it over, before the slot decides anything.
fn measured(id: Option<&str>, secs: f64) -> MeasuredDuration {
    MeasuredDuration::new(id.map(ToOwned::to_owned), secs)
}

#[test]
fn a_blank_id_names_no_track() {
    // The type claims `None` means "named no track"; minting holds it to that. Left alone, a
    // blank would satisfy `same_track` against the next blank and lend one length to another.
    assert_eq!(measured(Some(""), 200.0).track_id, None);
    assert_eq!(measured(Some("   "), 200.0).track_id, None);
    assert_eq!(measured(None, 200.0).track_id, None);
}

#[test]
fn a_measurement_naming_no_track_does_not_evict_one_that_does() {
    // A recover through `retained_product_id` can still measure with no id to give; the
    // gapless advance no longer does. Storing an untagged one would cost A the length a frame
    // had yet to claim, and buy nothing: an untagged measurement matches no frame. The Connect
    // metadata task once wiped a decoded length this way each time repeat-one re-announced the
    // track that was playing.
    let slot = settle_recorded_duration(Some(measured(Some("A"), 300.0)), measured(None, 250.0));

    assert_eq!(slot.as_ref().and_then(|m| m.track_id.as_deref()), Some("A"));
    assert_eq!(slot.map(|m| m.secs), Some(300.0));
}

#[test]
fn a_fresh_measurement_of_a_named_track_replaces_the_earlier_one() {
    // A quality swap or a recover re-measures the same track, and the later figure wins.
    let slot =
        settle_recorded_duration(Some(measured(Some("A"), 300.0)), measured(Some("A"), 301.5));

    assert_eq!(slot.map(|m| m.secs), Some(301.5));
}

#[test]
fn js_string_literal_escapes_line_and_paragraph_separators() {
    // serde_json leaves U+2028/U+2029 raw (RFC 8259), but they are JS line
    // terminators; \u-escape them for the literal to stay valid in any position.
    assert_eq!(js_string_literal("a\u{2028}b"), "\"a\\u2028b\"");
    assert_eq!(js_string_literal("a\u{2029}b"), "\"a\\u2029b\"");
}

#[test]
fn js_ipc_response_keeps_malicious_id_inside_one_string_literal() {
    // The audit payload tries to break out of both quote styles.
    let malicious = "x\");evil();//' or '";
    let js = js_ipc_response(malicious, "[1,2]");
    assert!(js.starts_with("window.__TIDAL_IPC_RESPONSE__("));
    assert!(js.ends_with(", null, [1,2])"));
    // The id argument must be a single JSON string literal that decodes back
    // to the exact input - proving it can never escape into code position.
    let first_arg = js
        .strip_prefix("window.__TIDAL_IPC_RESPONSE__(")
        .and_then(|s| s.strip_suffix(", null, [1,2])"))
        .expect("response structure intact");
    let decoded: String =
        serde_json::from_str(first_arg).expect("first arg is a valid JSON string literal");
    assert_eq!(decoded, malicious);
}

/// What the page had queued for itself: a position and a state, both its own track's.
fn queued_for_the_page() -> (Vec<PlayerBridgeEvent>, Option<(f64, u32)>) {
    (
        vec![
            PlayerBridgeEvent::time(1.0, 1),
            PlayerBridgeEvent::state("active", 1, LoadOrigin::Local),
        ],
        Some((1.5, 1)),
    )
}

#[tokio::test]
async fn the_end_of_a_page_stops_the_music_it_loaded_and_drops_what_it_queued() {
    // Every writer of the load generation holds this: a mint from another test cannot land
    // between the one under test and the read that checks it.
    let _serialised = crate::audio::preload::tests::PRELOAD_TESTS.lock().await;
    let player = Player::new(|_| {}, tokio::runtime::Handle::current());
    let local = begin_load(LoadOrigin::Local);
    let (mut events, mut time) = queued_for_the_page();

    end_page_music(&player, &mut events, &mut time);
    let after = current_load();

    begin_load(LoadOrigin::Local);
    assert_eq!(
        after,
        (local.wrapping_add(1), LoadOrigin::Local),
        "a stop was minted"
    );
    assert!(
        events.is_empty(),
        "the dead page's events were kept for its successor"
    );
    assert_eq!(
        time, None,
        "the dead page's position was kept for its successor"
    );
}

#[tokio::test]
async fn the_end_of_a_page_leaves_a_cast_playing() {
    let _serialised = crate::audio::preload::tests::PRELOAD_TESTS.lock().await;
    let player = Player::new(|_| {}, tokio::runtime::Handle::current());
    let cast = begin_load(LoadOrigin::Connect);
    let (mut events, mut time) = queued_for_the_page();

    end_page_music(&player, &mut events, &mut time);
    let after = current_load();

    begin_load(LoadOrigin::Local);
    assert_eq!(after, (cast, LoadOrigin::Connect), "no stop was minted");
    assert!(
        events.is_empty() && time.is_none(),
        "what the page queued is its own either way"
    );
}

/// A reload keeps the session, not one plugin of the page: the plugins the old page left waiting
/// on a store move would be injected a second time by the new page's own pass.
#[test]
fn a_new_page_drops_the_plugins_the_old_one_left_waiting() {
    let mut page = PagePhase::default();
    page.next();
    page.wait([("https://x/A".to_string(), "A".to_string())].into());

    page.next();

    assert!(
        page.take_waiting().is_empty(),
        "the old page's waiting plugins carried over"
    );
}

/// A store move's answer loads its plugins only once a pass tried every plugin, counting the rest
/// as live. A later pass that could not prepare one takes that back: the answer after it would
/// load a dependant against a library that never started.
#[test]
fn the_last_pass_of_a_page_says_whether_its_plugins_were_all_tried() {
    let mut page = PagePhase::default();
    let ticket = Ticket {
        epoch: 1,
        page: page.next(),
    };
    let other = Ticket {
        epoch: 2,
        page: ticket.page,
    };
    assert_eq!(page.pass_stood(ticket), PassStood::NotYet);

    page.mark_loaded(ticket);
    assert_eq!(page.pass_stood(ticket), PassStood::Completed);
    assert_eq!(
        page.pass_stood(other),
        PassStood::NotYet,
        "a pass of another session vouched for this one"
    );

    page.mark_incomplete(ticket);
    assert_eq!(
        page.pass_stood(ticket),
        PassStood::Incomplete,
        "an earlier complete pass outweighed the later one that could not load everything"
    );
}

/// A state whose current token generation is `prior`, in session 1.
fn state_with_tokens(prior: Option<TokenGeneration>) -> AppState {
    let player = Player::new(|_| {}, tokio::runtime::Handle::current());
    let mut state = AppState::new(Arc::new(player));
    state.session_epoch = 1;
    state.token_state = prior.map(|current| StoredTokenState {
        current,
        previous: None,
        previous_valid_until: None,
    });
    state
}

fn generation(refresh_token: &str, client_id: &str, version: u64) -> TokenGeneration {
    TokenGeneration {
        access_token: "old-at".into(),
        refresh_token: refresh_token.into(),
        opaque_at: "old-opaque-at".into(),
        opaque_rt: "old-opaque-rt".into(),
        version,
        access_expires: 0,
        user_id: None,
        granted_scopes: Vec::new(),
        client: ClientBinding::Proven(client_id.into()),
    }
}

/// An exchange answer, carrying a new refresh token when `refresh_token` is set.
fn grant(refresh_token: Option<&str>) -> TokenGrant<'_> {
    TokenGrant {
        access_token: "new-at",
        refresh_token,
        opaque_at: "new-opaque-at",
        opaque_rt: refresh_token.map(|_| "new-opaque-rt"),
        expires_in: None,
        user_id: None,
        granted_scopes: &[],
        client_id: None,
    }
}

#[tokio::test]
async fn a_token_answer_for_an_ended_session_writes_nothing() {
    let mut state = state_with_tokens(Some(generation("old-rt", "cid", 4)));

    let commit = state.apply_token_generation(0, &grant(Some("new-rt")), UserCommit::Login);

    assert!(matches!(commit, TokenCommit::Refused));
    let current = &state.token_state.as_ref().unwrap().current;
    assert_eq!(
        (current.version, current.refresh_token.as_str()),
        (4, "old-rt"),
        "a departed session's answer replaced the live generation"
    );
    assert_eq!(
        state.access_token(),
        Some("old-at"),
        "a departed session's bearer token became the live one"
    );
}

#[tokio::test]
async fn a_new_generation_keeps_the_one_it_replaces_for_thirty_seconds() {
    let mut state = state_with_tokens(Some(generation("old-rt", "cid", 4)));

    let commit = state.apply_token_generation(1, &grant(Some("new-rt")), UserCommit::Login);

    assert!(matches!(
        commit,
        TokenCommit::Committed { durable: true, .. }
    ));
    let tokens = state.token_state.as_ref().unwrap();
    assert_eq!(
        (
            tokens.current.version,
            tokens.current.refresh_token.as_str()
        ),
        (5, "new-rt")
    );
    assert_eq!(
        tokens.previous.as_ref().map(|g| g.version),
        Some(4),
        "requests still carrying the replaced token lost it at once"
    );
}

/// A plugin's request carries the stored token only under the session that issued its capability,
/// and the session check comes first: a stale plugin learns nothing of whether a token exists.
#[tokio::test]
async fn a_plugin_gets_the_stored_token_only_under_its_own_session() {
    let with_token = state_with_tokens(Some(generation("rt", "cid", 1)));
    let mut empty = generation("rt", "cid", 1);
    empty.access_token = String::new();
    let with_empty = state_with_tokens(Some(empty));
    let without = state_with_tokens(None);

    for (state, capability_epoch, expected, why) in [
        (
            &with_token,
            Some(1),
            PluginCredential::Credential("old-at".into()),
            "the session that issued the capability holds a token",
        ),
        (
            &with_token,
            Some(0),
            PluginCredential::PastSession,
            "the capability belongs to a session the user left",
        ),
        (
            &with_token,
            None,
            PluginCredential::PastSession,
            "an unknown capability is no session's",
        ),
        (
            &without,
            Some(1),
            PluginCredential::NoCredential,
            "no session holds a token",
        ),
        (
            &with_empty,
            Some(1),
            PluginCredential::NoCredential,
            "an empty token is no token",
        ),
    ] {
        let credential = PluginCredential::for_plugin(
            capability_epoch,
            state.session_epoch,
            state.token_state.as_ref(),
        );
        assert_eq!(credential, expected, "{why}");
    }
}

#[tokio::test]
async fn an_answer_without_a_refresh_token_keeps_the_stored_one() {
    let mut state = state_with_tokens(Some(generation("old-rt", "cid", 4)));

    let commit = state.apply_token_generation(1, &grant(None), UserCommit::Refresh);

    let TokenCommit::Committed { durable, .. } = commit else {
        panic!("a live session's answer was refused");
    };
    assert!(durable, "the stored refresh token was dropped");
    let current = &state.token_state.as_ref().unwrap().current;
    assert_eq!(current.opaque_rt, "old-opaque-rt");
    assert_eq!(current.refresh_token, "old-rt");
}

#[tokio::test]
async fn a_generation_without_any_refresh_token_is_not_durable() {
    // Saved anyway, it would replace a credential that can refresh with one that cannot.
    let mut state = state_with_tokens(None);

    let commit = state.apply_token_generation(1, &grant(None), UserCommit::Refresh);

    assert!(matches!(
        commit,
        TokenCommit::Committed { durable: false, .. }
    ));
}

/// A login after the app-level generation stores the user's client_id with the user's refresh
/// token: a mismatched pair fails the next refresh with `invalid_client`.
#[tokio::test]
async fn a_commit_stores_the_client_id_its_refresh_token_was_minted_under() {
    let mut state = state_with_tokens(Some(generation("", "app", 0)));
    let login = TokenGrant {
        client_id: Some("user"),
        ..grant(Some("user-rt"))
    };
    state.apply_token_generation(1, &login, UserCommit::Login);
    assert_eq!(
        state.token_state.as_ref().unwrap().current.client.id(),
        "user",
        "the login's refresh token takes the login's client"
    );
}

/// A client is proven only by TIDAL accepting it for the token. A sign-in proves the client
/// its exchange was seen sent with, a refresh the client it was sent with; a refresh sent without
/// one keeps the binding, and a sign-in whose client went unseen keeps the prior id, unproven.
#[test]
fn a_client_is_proven_only_by_an_answer_to_a_request_sent_with_it() {
    let unproven = ClientBinding::Unproven("store".into());
    let proven = ClientBinding::Proven("store".into());
    for (prior, sent, login, refresh) in [
        (
            Some(&unproven),
            Some("page"),
            ClientBinding::Proven("page".into()),
            ClientBinding::Proven("page".into()),
        ),
        (Some(&proven), Some("store"), proven.clone(), proven.clone()),
        (Some(&unproven), None, unproven.clone(), unproven.clone()),
        (Some(&proven), None, unproven.clone(), proven.clone()),
        (Some(&proven), Some(""), unproven.clone(), proven.clone()),
        (
            None,
            Some("page"),
            ClientBinding::Proven("page".into()),
            ClientBinding::Proven("page".into()),
        ),
        (
            None,
            None,
            ClientBinding::Unproven(String::new()),
            ClientBinding::Unproven(String::new()),
        ),
    ] {
        assert_eq!(
            ClientBinding::after_login(prior, sent),
            login,
            "login, {prior:?} sent {sent:?}"
        );
        assert_eq!(
            ClientBinding::after_refresh(prior, sent),
            refresh,
            "refresh, {prior:?} sent {sent:?}"
        );
    }
}

fn user_generation(user: Option<&str>) -> TokenGeneration {
    TokenGeneration {
        user_id: user.map(str::to_owned),
        ..generation("user-rt", "cid", 4)
    }
}

/// A refresh for another account writes nothing: committed, it would switch the SDK to that
/// account. A store without a user, or the same user, takes it.
#[tokio::test]
async fn a_refresh_for_another_account_writes_nothing() {
    for (stored_user, answered_user, commits, why) in [
        (Some("a"), Some("b"), false, "another account"),
        (Some("a"), Some("a"), true, "the same account"),
        (None, Some("b"), true, "a store that names no user"),
    ] {
        let mut state = state_with_tokens(Some(user_generation(stored_user)));
        let refresh = TokenGrant {
            user_id: answered_user,
            ..grant(None)
        };

        let commit = state.apply_token_generation(1, &refresh, UserCommit::Refresh);

        let current = &state.token_state.as_ref().unwrap().current;
        if commits {
            assert!(
                matches!(commit, TokenCommit::Committed { ended: None, .. }),
                "{why}"
            );
            assert_eq!(current.access_token, "new-at", "{why}");
        } else {
            assert!(matches!(commit, TokenCommit::SwappedAccount), "{why}");
            assert_eq!(
                (current.access_token.as_str(), current.version),
                ("old-at", 4),
                "{why}"
            );
        }
    }
}

/// A sign-in to another account ends the session in the same section: the epoch moves on, and the
/// new generation keeps nothing of the departed one, neither its refresh token nor a previous.
#[tokio::test]
async fn a_sign_in_to_another_account_ends_the_session_first() {
    let mut state = state_with_tokens(Some(user_generation(Some("a"))));
    let login = TokenGrant {
        user_id: Some("b"),
        client_id: Some("cid-b"),
        ..grant(Some("b-rt"))
    };

    let commit = state.apply_token_generation(1, &login, UserCommit::Login);

    assert!(matches!(
        commit,
        TokenCommit::Committed {
            ended: Some(_),
            durable: true
        }
    ));
    assert_eq!(state.session_epoch, 2);
    let tokens = state.token_state.as_ref().unwrap();
    assert_eq!(tokens.current.user_id.as_deref(), Some("b"));
    assert_eq!(tokens.current.refresh_token, "b-rt");
    assert_eq!(tokens.current.version, 1, "numbered from the empty state");
    assert!(
        tokens.previous.is_none(),
        "the departed account's token stayed usable"
    );

    let mut same = state_with_tokens(Some(user_generation(Some("a"))));
    let again = TokenGrant {
        user_id: Some("a"),
        ..grant(Some("a-rt"))
    };
    let commit = same.apply_token_generation(1, &again, UserCommit::Login);
    assert!(matches!(commit, TokenCommit::Committed { ended: None, .. }));
    assert_eq!(
        same.session_epoch, 1,
        "a sign-in to the same account ended its session"
    );
}

#[tokio::test]
async fn an_app_token_is_renewed_in_place_under_the_pages_nonce() {
    let mut state = state_with_tokens(Some(user_generation(Some("a"))));
    let first = TokenGrant {
        access_token: "app-at-1",
        opaque_at: "luna_app_1",
        ..grant(None)
    };
    let second = TokenGrant {
        access_token: "app-at-2",
        opaque_at: "luna_app_2",
        ..grant(None)
    };

    assert_eq!(state.apply_app_token(&first), "luna_app_1");
    assert_eq!(
        state.apply_app_token(&second),
        "luna_app_1",
        "the page's nonce went stale"
    );

    let app = state.app_token.as_ref().unwrap();
    assert_eq!(
        (app.opaque_at.as_str(), app.access_token.as_str()),
        ("luna_app_1", "app-at-2")
    );
}
