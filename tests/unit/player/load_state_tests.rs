//! The load identity: a generation and the origin that minted it, in one word.
//!
//! These tests pin the rule on the primitive rather than on any of its readers. Held apart in
//! two atomics, the pair could be published half-and-half, and the only reader that weighs both
//! -`next_is_current`- then admitted a renderer-staged track into someone else's session.

use super::fixture::silent_player;
use super::{Continuation, LoadOrigin, StopScope, begin_load, current_load};

/// Every writer of the load generation in the suite holds this: a mint from another test
/// cannot land between one of ours and the read that checks it.
async fn serialised() -> tokio::sync::MutexGuard<'static, ()> {
    crate::audio::preload::tests::PRELOAD_TESTS.lock().await
}

/// Two minters racing must never leave a generation readable beside the other one's origin.
///
/// This is the defect the one word exists to close. As two atomics, the origin store and the
/// generation bump interleaved: the newest generation belonged to the Connect receiver while the
/// origin still read `Local`, durably, since nothing rewrote the pair until the next mint;
/// `next_is_current` waved a renderer-staged track into a Connect session on the strength of it.
///
/// Each minter checks its OWN mint, which needs no bookkeeping to be conclusive: a generation is
/// installed by exactly one mint. While it is still current, the origin beside it can only be
/// the one that minted it. The rounds are not the proof (one word makes every observation a
/// whole mint); they are what made the two-atomic shape fail.
#[tokio::test]
async fn two_minters_never_leave_a_generation_beside_the_other_origin() {
    let _serialised = serialised().await;

    const ROUNDS: usize = 20_000;
    let minters: Vec<_> = [LoadOrigin::Local, LoadOrigin::Connect]
        .into_iter()
        .map(|origin| {
            std::thread::spawn(move || {
                for _ in 0..ROUNDS {
                    let minted = begin_load(origin);
                    let (cur_gen, observed) = current_load();
                    assert!(
                        cur_gen != minted || observed == origin,
                        "generation {minted} read back beside {observed:?}, minted by {origin:?}"
                    );
                }
            })
        })
        .collect();

    for minter in minters {
        minter.join().expect("a minter observed a mispaired word");
    }

    // Process-wide: hand the local origin back before releasing the lock, or a later test reads
    // whichever origin happened to mint last.
    begin_load(LoadOrigin::Local);
}

/// Re-armed in this host's name, a cast recovered from a device fault handed the player to the
/// renderer, whose next load replaced the controller's track.
#[tokio::test]
async fn a_rearm_keeps_the_owner_of_the_load_it_rearms() {
    let _serialised = serialised().await;
    let _slot = crate::state::current_track_fixture::CurrentTrackSlot::clear();
    let player = silent_player();

    let cast = begin_load(LoadOrigin::Connect);
    let outcome = rearm_of(&player, cast);
    let owner = current_load();

    begin_load(LoadOrigin::Local);
    assert_eq!(outcome, Continuation::Started);
    assert_eq!(
        owner,
        (cast.wrapping_add(1), LoadOrigin::Connect),
        "the controller's track, re-armed, still belongs to the controller"
    );
}

/// A re-arm of the load `expected_gen`, the way the `ReplayRequest` consumer issues it.
fn rearm_of(player: &super::Player, expected_gen: u32) -> Continuation {
    player.rearm(
        // Nothing listens there: the re-armed load fails on its own, off this test's path.
        "http://127.0.0.1:9/mediatracks/cast.flac".to_string(),
        "flac".to_string(),
        String::new(),
        Some("1".to_string()),
        None,
        false,
        expected_gen,
    )
}

/// A session release landing between the re-arm's look at the load and its mint was overridden:
/// the renderer's loads were then refused for a controller already gone.
#[tokio::test]
async fn a_rearm_behind_a_session_release_does_not_claim_the_player_back() {
    let _serialised = serialised().await;
    let _slot = crate::state::current_track_fixture::CurrentTrackSlot::clear();
    let player = silent_player();

    let cast = begin_load(LoadOrigin::Connect);
    // The controller's session ends before the re-arm runs.
    player
        .stop(StopScope::Cast)
        .expect("the player thread takes the stop");
    let released = current_load();
    let outcome = rearm_of(&player, cast);
    let after = current_load();

    begin_load(LoadOrigin::Local);
    assert_eq!(released, (cast.wrapping_add(1), LoadOrigin::Local));
    assert_eq!(
        after, released,
        "the re-arm restarted a released track in a finished session's name"
    );
    assert_eq!(outcome, Continuation::Superseded);
}

#[tokio::test]
async fn the_advance_does_not_start_over_a_cast_that_took_the_player_after_the_take() {
    let _serialised = serialised().await;
    let _slot = crate::state::current_track_fixture::CurrentTrackSlot::clear();
    let player = silent_player();

    let local = begin_load(LoadOrigin::Local);
    let next = staged_next(local);
    // A controller takes the player between the take and the advance's mint.
    let cast = begin_load(LoadOrigin::Connect);
    let outcome = player.advance(next);
    let after = current_load();

    begin_load(LoadOrigin::Local);
    assert_eq!(
        after,
        (cast, LoadOrigin::Connect),
        "the cast lost the player"
    );
    assert_eq!(outcome, Continuation::Superseded);
}

/// A staged next as `take_next_track` hands it over, validated against `load_gen`.
fn staged_next(load_gen: u32) -> crate::state::RetainedTrack {
    crate::state::RetainedTrack {
        track: crate::state::TrackInfo {
            url: "http://127.0.0.1:9/mediatracks/next.flac".to_string(),
            format: "flac".to_string(),
            key: String::new(),
            product_id: Some("2".to_string()),
        },
        load_gen,
    }
}

/// Every scope against every owner: what a stop may halt, and whose the player is after it.
#[test]
fn a_stop_halts_only_the_music_its_scope_covers() {
    use LoadOrigin::{Connect, Local};
    let table = [
        (StopScope::As(Local), Local, Some(Local)),
        (StopScope::As(Local), Connect, Some(Local)),
        (StopScope::As(Connect), Local, Some(Connect)),
        (StopScope::As(Connect), Connect, Some(Connect)),
        (StopScope::Playing, Local, Some(Local)),
        (StopScope::Playing, Connect, Some(Connect)),
        (StopScope::OwnMusic, Local, Some(Local)),
        (StopScope::OwnMusic, Connect, None),
        (StopScope::Cast, Local, None),
        (StopScope::Cast, Connect, Some(Local)),
    ];
    for (scope, owner, minted_as) in table {
        assert_eq!(scope.claim(owner), minted_as, "{scope:?} over {owner:?}");
    }
}

/// A stop mints only when its scope covers the owner: the OS controls' stop leaves a cast its
/// owner, while a page session ending over a cast, or the release of a session that never cast,
/// used to stop this host's own music anyway and now mints nothing.
#[tokio::test]
async fn a_stop_mints_only_when_its_scope_covers_the_owner() {
    let _serialised = serialised().await;
    let player = silent_player();

    for (owner, scope, mints) in [
        (LoadOrigin::Connect, StopScope::Playing, true),
        (LoadOrigin::Connect, StopScope::OwnMusic, false),
        (LoadOrigin::Local, StopScope::Cast, false),
    ] {
        let load = begin_load(owner);
        player
            .stop(scope)
            .expect("the player thread takes the stop");
        let after = current_load();

        begin_load(LoadOrigin::Local);
        let expected = if mints { load.wrapping_add(1) } else { load };
        assert_eq!(after, (expected, owner), "{scope:?} over {owner:?}");
    }
}

/// A stop leaves the track retained and resumable; only the end of a page leaves it with no page
/// behind it, and the player thread is told so for that scope alone.
#[test]
fn only_a_page_ending_leaves_the_music_it_stops_without_a_page() {
    use LoadOrigin::{Connect, Local};
    let table = [
        (StopScope::As(Local), false),
        (StopScope::As(Connect), false),
        (StopScope::Playing, false),
        (StopScope::OwnMusic, true),
        (StopScope::Cast, false),
    ];
    for (scope, orphans) in table {
        assert_eq!(scope.orphans(), orphans, "{scope:?}");
    }
}
