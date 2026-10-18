//! Tests for `src/app_state.rs`, attached to it by `#[path]`.

use super::{
    MeasuredDuration, end_page_music, js_ipc_response, js_string_literal, settle_recorded_duration,
};
use crate::bridge::PlayerBridgeEvent;
use crate::player::{LoadOrigin, Player, begin_load, current_load};

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
fn js_string_literal_is_a_json_string() {
    assert_eq!(js_string_literal("a\"b\\c\nd"), r#""a\"b\\c\nd""#);
}

#[test]
fn js_string_literal_escapes_line_and_paragraph_separators() {
    // serde_json leaves U+2028/U+2029 raw (RFC 8259), but they are JS line
    // terminators; \u-escape them for the literal to stay valid in any position.
    assert_eq!(js_string_literal("a\u{2028}b"), "\"a\\u2028b\"");
    assert_eq!(js_string_literal("a\u{2029}b"), "\"a\\u2029b\"");
}

#[test]
fn js_string_literal_leaves_an_apostrophe_alone() {
    // The literal is double-quoted; an apostrophe needs nothing done to it. A caller that
    // escaped one first (`updater.error` did, from back when it built the statement itself
    // with single quotes) hands the encoder a backslash, which the encoder then escapes as
    // the character it is: the user reads "Run \'sudo apt upgrade tidalunar\'". Escaping
    // belongs to the encoder alone, and this is the character that says so.
    assert_eq!(
        js_string_literal("Run 'apt upgrade'"),
        r#""Run 'apt upgrade'""#
    );
    assert_eq!(js_string_literal(r"pre\'escaped"), r#""pre\\'escaped""#);
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
