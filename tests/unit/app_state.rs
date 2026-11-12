//! Tests for `src/app_state.rs`, attached to it by `#[path]`.

use std::sync::Arc;

use super::{
    AppState, MeasuredDuration, PagePhase, PassStood, Ticket, TokenCommit, TokenGrant,
    end_page_music, js_ipc_response, js_string_literal, settle_recorded_duration,
};
use crate::bridge::PlayerBridgeEvent;
use crate::platform::secure_store::{StoredTokenState, TokenGeneration};
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
        client_id: client_id.into(),
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

    let commit = state.apply_token_generation(0, &grant(Some("new-rt")));

    assert!(matches!(commit, TokenCommit::Refused));
    let current = &state.token_state.as_ref().unwrap().current;
    assert_eq!(
        (current.version, current.refresh_token.as_str()),
        (4, "old-rt"),
        "a departed session's answer replaced the live generation"
    );
    assert!(
        state.captured_token.is_empty(),
        "a departed session's bearer token became the live one"
    );
}

#[tokio::test]
async fn a_new_generation_keeps_the_one_it_replaces_for_thirty_seconds() {
    let mut state = state_with_tokens(Some(generation("old-rt", "cid", 4)));

    let before = now_unix_secs();
    let commit = state.apply_token_generation(1, &grant(Some("new-rt")));
    let after = now_unix_secs();

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
    let until = tokens.previous_valid_until.unwrap();
    assert!((before + 30..=after + 30).contains(&until), "got {until}");
    assert_eq!(state.captured_token, "new-at");
}

#[tokio::test]
async fn an_answer_without_a_refresh_token_keeps_the_stored_one() {
    let mut state = state_with_tokens(Some(generation("old-rt", "cid", 4)));

    let commit = state.apply_token_generation(1, &grant(None));

    let TokenCommit::Committed { opaque_rt, durable } = commit else {
        panic!("a live session's answer was refused");
    };
    assert_eq!(opaque_rt, "old-opaque-rt");
    assert!(durable, "the stored refresh token was dropped");
    assert_eq!(
        state.token_state.as_ref().unwrap().current.refresh_token,
        "old-rt"
    );
}

#[tokio::test]
async fn a_generation_without_any_refresh_token_is_not_durable() {
    // Saved anyway, it would replace a credential that can refresh with one that cannot.
    let mut state = state_with_tokens(None);

    let commit = state.apply_token_generation(1, &grant(None));

    assert!(matches!(
        commit,
        TokenCommit::Committed { durable: false, .. }
    ));
}

/// A login after the app-level generation stores the user's client_id with the user's refresh
/// token; the app-level answer that follows keeps it. A mismatched pair fails the next refresh
/// with `invalid_client`.
#[tokio::test]
async fn a_commit_stores_the_client_id_its_refresh_token_was_minted_under() {
    for (prior, refresh_token, exchange, expected) in [
        (generation("", "app", 0), Some("user-rt"), "user", "user"),
        (generation("user-rt", "user", 0), None, "app", "user"),
    ] {
        let mut state = state_with_tokens(Some(prior));
        let answer = TokenGrant {
            client_id: Some(exchange),
            ..grant(refresh_token)
        };

        state.apply_token_generation(1, &answer);

        assert_eq!(
            state.token_state.as_ref().unwrap().current.client_id,
            expected,
            "new refresh: {refresh_token:?}, exchange: {exchange}"
        );
    }
}

/// The exchange's client_id binds only a NEW refresh token; an empty or missing one never blanks
/// the prior.
#[test]
fn a_generation_takes_the_exchange_client_id_only_with_a_new_refresh_token() {
    for (refresh_token, exchange, prior, expected) in [
        (Some("rt"), Some("user"), Some("app"), "user"),
        (None, Some("app"), Some("user"), "user"),
        (Some("rt"), None, Some("user"), "user"),
        (Some("rt"), Some(""), Some("user"), "user"),
        (Some("rt"), Some("user"), None, "user"),
        (None, Some("app"), None, "app"),
    ] {
        let answer = TokenGrant {
            client_id: exchange,
            ..grant(refresh_token)
        };
        assert_eq!(
            answer.bound_client_id(prior),
            expected,
            "new refresh: {refresh_token:?}, exchange: {exchange:?}, prior: {prior:?}"
        );
    }
}
