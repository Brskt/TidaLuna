//! The load identity: a generation and the origin that minted it, in one word.
//!
//! These tests pin the rule on the primitive rather than on any of its readers. Held apart in
//! two atomics, the pair could be published half-and-half, and the only reader that weighs both
//! -`next_is_current`- then admitted a renderer-staged track into someone else's session.

use super::fixture::silent_player;
use super::{
    Continuation, LoadOrigin, StopScope, begin_load, continue_load, current_gen, current_load,
    pack_load_state, unpack_load_state,
};

/// Every writer of the load generation in the suite holds this: a mint from another test
/// cannot land between one of ours and the read that checks it.
async fn serialised() -> tokio::sync::MutexGuard<'static, ()> {
    crate::audio::preload::tests::PRELOAD_TESTS.lock().await
}

#[test]
fn a_packed_word_gives_back_what_it_was_built_from() {
    for cur_gen in [0, 1, 2, 4096, u32::MAX - 1, u32::MAX] {
        for origin in [LoadOrigin::Local, LoadOrigin::Connect] {
            assert_eq!(
                unpack_load_state(pack_load_state(cur_gen, origin)),
                (cur_gen, origin),
                "the word lost a half"
            );
        }
    }
}

/// `Local` is zero. An untouched process answers with this host's own queue, which is what
/// every reader assumes before the first load.
#[test]
fn an_untouched_word_names_this_host_at_generation_zero() {
    assert_eq!(unpack_load_state(0), (0, LoadOrigin::Local));
}

/// The generation occupies the high half: it cannot collide with the origin byte however far
/// it climbs, and the origin cannot perturb the generation a reader compares against its stamp.
#[test]
fn the_two_halves_do_not_reach_into_each_other() {
    assert_eq!(
        unpack_load_state(pack_load_state(u32::MAX, LoadOrigin::Connect)).0,
        u32::MAX
    );
    assert_eq!(
        pack_load_state(7, LoadOrigin::Connect) - pack_load_state(7, LoadOrigin::Local),
        LoadOrigin::Connect as u64,
        "the origin must live entirely below the generation"
    );
}

#[tokio::test]
async fn a_mint_publishes_its_origin_with_its_generation() {
    let _serialised = serialised().await;

    let minted = begin_load(LoadOrigin::Connect);
    assert_eq!(current_load(), (minted, LoadOrigin::Connect));
    assert_eq!(current_gen(), minted, "both accessors read the one word");

    let next = begin_load(LoadOrigin::Local);
    assert_eq!(
        next,
        minted.wrapping_add(1),
        "a mint advances the generation"
    );
    assert_eq!(current_load(), (next, LoadOrigin::Local));
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
    player
        .rearm(
            // Nothing listens there: the re-armed load fails on its own, off this test's path.
            "http://127.0.0.1:9/mediatracks/cast.flac".to_string(),
            "flac".to_string(),
            String::new(),
            Some("1".to_string()),
            None,
            false,
            expected_gen,
        )
        .expect("a re-arm reaches the load path")
}

#[tokio::test]
async fn a_continuation_mints_only_over_the_load_it_continues() {
    let _serialised = serialised().await;

    let cast = begin_load(LoadOrigin::Connect);
    let continued = continue_load(cast);
    let after_first = current_load();
    let again = continue_load(cast);
    let after_second = current_load();

    begin_load(LoadOrigin::Local);
    assert_eq!(continued, Some(cast.wrapping_add(1)));
    assert_eq!(
        after_first,
        (cast.wrapping_add(1), LoadOrigin::Connect),
        "the owner rides the continuation"
    );
    assert_eq!(again, None, "the load it named is no longer current");
    assert_eq!(
        after_second, after_first,
        "a declined continuation writes nothing"
    );
}

/// A session release landing between the re-arm's look at the load and its mint was overridden:
/// the released track restarted in a finished session's name.
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
    let outcome = player
        .advance(next)
        .expect("an advance reaches the load path");
    let after = current_load();

    begin_load(LoadOrigin::Local);
    assert_eq!(
        after,
        (cast, LoadOrigin::Connect),
        "the cast lost the player"
    );
    assert_eq!(outcome, Continuation::Superseded);
}

#[tokio::test]
async fn the_advance_still_moves_this_hosts_own_queue_on() {
    let _serialised = serialised().await;
    let _slot = crate::state::current_track_fixture::CurrentTrackSlot::clear();
    let player = silent_player();

    let local = begin_load(LoadOrigin::Local);
    let outcome = player
        .advance(staged_next(local))
        .expect("an advance reaches the load path");
    let after = current_load();

    begin_load(LoadOrigin::Local);
    assert_eq!(outcome, Continuation::Started);
    assert_eq!(after, (local.wrapping_add(1), LoadOrigin::Local));
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

#[tokio::test]
async fn a_stop_from_the_os_controls_leaves_a_cast_its_owner() {
    let _serialised = serialised().await;
    let player = silent_player();

    let cast = begin_load(LoadOrigin::Connect);
    player
        .stop(StopScope::Playing)
        .expect("the player thread takes the stop");
    let after = current_load();

    begin_load(LoadOrigin::Local);
    assert_eq!(after, (cast.wrapping_add(1), LoadOrigin::Connect));
}

#[tokio::test]
async fn a_page_session_ending_leaves_a_cast_playing() {
    let _serialised = serialised().await;
    let player = silent_player();

    let cast = begin_load(LoadOrigin::Connect);
    player
        .stop(StopScope::OwnMusic)
        .expect("a declined stop is not a failure");
    let after = current_load();

    begin_load(LoadOrigin::Local);
    assert_eq!(after, (cast, LoadOrigin::Connect), "no stop was minted");
}

/// The release of a session that never cast used to stop this host's own music anyway.
#[tokio::test]
async fn a_session_that_never_cast_leaves_local_music_playing() {
    let _serialised = serialised().await;
    let player = silent_player();

    let local = begin_load(LoadOrigin::Local);
    player
        .stop(StopScope::Cast)
        .expect("a declined stop is not a failure");
    let after = current_load();

    begin_load(LoadOrigin::Local);
    assert_eq!(after, (local, LoadOrigin::Local), "no stop was minted");
}
