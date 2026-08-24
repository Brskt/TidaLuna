//! Tests for `src/audio/bandwidth.rs`, attached to it by `#[path]`.

use super::*;

/// The boost's entry path. Its trigger was lost in a buffer rewrite, leaving the flag readable
/// and unsettable: the governor then only ever reached the boosted rate through the starvation
/// watchdog, hundreds of milliseconds later. What this pins is the property, not the plumbing:
/// arming the request raises the playback rate on the next tick.
#[test]
fn arming_the_seek_boost_raises_the_playback_rate() {
    let progress = BufferProgress::new();
    progress.bitrate_bps.store(100_000, Relaxed);
    let mut governor = GovernorState::new();
    let unboosted = governor.playback_bucket.rate;

    governor.update_boost(&progress);
    assert_eq!(
        governor.playback_bucket.rate, unboosted,
        "nothing asked for the boost; the tick leaves the rate alone"
    );

    progress.request_seek_boost();
    governor.update_boost(&progress);

    assert_eq!(
        governor.playback_bucket.rate,
        100_000.0 * BOOST_MULT,
        "a restart that has to refetch is what the boosted rate exists for"
    );
}

/// The flag is a request, not a state: the governor consumes it and one arming cannot keep the
/// rate raised past the exit conditions.
#[test]
fn the_seek_boost_request_is_consumed_by_the_tick_that_reads_it() {
    let progress = BufferProgress::new();
    progress.request_seek_boost();

    assert!(progress.take_seek_boost(), "the request reaches its reader");
    assert!(
        !progress.take_seek_boost(),
        "a second reader must not inherit an arming already answered"
    );
}
