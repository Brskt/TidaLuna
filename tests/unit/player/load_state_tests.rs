//! The load identity: a generation and the origin that minted it, in one word.
//!
//! These tests pin the rule on the primitive rather than on any of its readers. Held apart in
//! two atomics, the pair could be published half-and-half, and the only reader that weighs both
//! -`next_is_current`- then admitted a renderer-staged track into someone else's session.

use super::{
    LoadOrigin, begin_load, current_gen, current_load, pack_load_state, unpack_load_state,
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
