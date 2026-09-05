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

/// A preload queued behind a shut gate, sized so the caller can be told apart from the
/// budget. `bytes` and `remaining` start equal, the way `acquire` builds one.
fn queued_preload(bytes: u32) -> (VecDeque<TokenRequest>, oneshot::Receiver<()>) {
    let (reply, rx) = oneshot::channel();
    let mut queue = VecDeque::new();
    queue.push_back(TokenRequest {
        class: TrafficClass::Preload,
        bytes,
        remaining: bytes,
        reply,
    });
    (queue, rx)
}

/// The state a track is in while its own download is paced at real time: playing, a second
/// of audio ahead, which is under the gate's shut threshold.
fn playing_with_a_collapsed_lead(bitrate: u64, total_len: u64) -> BufferProgress {
    let bp = BufferProgress::new();
    bp.set_playback_active(true);
    bp.set_crossfade_secs(6);
    bp.bitrate_bps.store(bitrate, Relaxed);
    bp.total_len.store(total_len, Relaxed);
    bp.written.store(bitrate / 2, Relaxed);
    bp.read_pos.store(0, Relaxed);
    bp
}

/// A reply only goes out at `remaining == 0`; a request the budget cuts in half leaves its
/// caller parked on bytes it was already charged for, holding the head of the queue (the
/// only position either server looks at). The budget therefore decides WHICH requests
/// start, and a started one is finished whatever it costs.
#[test]
fn a_request_the_budget_cannot_fit_is_finished_rather_than_left_part_paid() {
    let (mut queue, mut rx) = queued_preload(100_000);
    let mut bucket = TokenBucket::new(1_000_000.0, 1_000_000.0);
    let mut served = 0u64;

    let granted = serve_queue_capped(&mut queue, &mut bucket, &mut served, 40_000);

    assert_eq!(
        granted, 100_000,
        "the budget is overshot by the tail of one request, never by more"
    );
    assert!(queue.is_empty(), "nothing is left holding the head");
    assert!(
        rx.try_recv().is_ok(),
        "the downloader gets the bytes it was charged for"
    );
    assert_eq!(served, 100_000, "and they are counted once, on completion");
}

/// The budget decides which requests may START; it must never strand one that already did.
///
/// A reply only goes out at `remaining == 0`, and both servers look only at the front of the
/// queue. A request left part-paid does not merely lose its own bytes, it blocks every
/// preload behind it until the track ends. Gating the CALL on unspent allowance put that
/// decision where it could not see a started request, and the only pass able to finish one
/// stopped running exactly when it was needed.
///
/// Built on the real constants deliberately. The tests above hand themselves a bucket thirty
/// times `PRELOAD_BURST`, or ask for less than one burst: every request completes inside a
/// single call, and the multi-tick path, the only place this defect lives, never runs.
#[test]
fn a_request_started_before_the_budget_ran_out_is_finished_on_a_later_tick() {
    let bp = playing_with_a_collapsed_lead(100_000, 10_000_000);
    let mut governor = GovernorState::new();
    governor.gate = PreloadGate::Paused;
    governor.tick(&bp);

    // Nearly all of the fade's allowance is spent: less than one burst of budget is left,
    // which is the state of whichever request happens to straddle the limit.
    let allowance = bp.head_allowance();
    governor.preload_head_granted = allowance - 20_000;

    // Larger than PRELOAD_BURST, which is what makes it span two ticks. reqwest yields 16 to
    // 64 KB at a time: this is an ordinary chunk, not a contrived one.
    let (queue, mut rx) = queued_preload(60_000);
    governor.preload_queue = queue;

    governor.tick(&bp);

    assert!(
        rx.try_recv().is_err(),
        "one burst cannot cover it, so it is still owed bytes here"
    );
    assert!(
        governor.preload_head_granted > allowance,
        "and the request it started has carried the total past the allowance"
    );

    // A later tick, with the bucket refilled by elapsed time.
    governor.preload_bucket.tokens = PRELOAD_BURST;
    governor.tick(&bp);

    assert!(
        rx.try_recv().is_ok(),
        "a started request is finished whatever the budget says: leaving it owed wedges the \
         head of the queue for the rest of the track"
    );
    assert!(
        governor.preload_queue.is_empty(),
        "and the queue is free for whatever comes next"
    );
}

/// The overshoot is bounded by one request: once the budget is spent, the next request
/// waits rather than starting.
#[test]
fn a_request_that_has_not_started_waits_for_the_budget() {
    let (mut queue, mut rx) = queued_preload(16_000);
    let mut bucket = TokenBucket::new(1_000_000.0, 1_000_000.0);
    let mut served = 0u64;

    let granted = serve_queue_capped(&mut queue, &mut bucket, &mut served, 0);

    assert_eq!(granted, 0, "no budget, no start");
    assert_eq!(queue.len(), 1, "it keeps its place for the next track");
    assert!(rx.try_recv().is_err());
}

/// The allowance is per track, and a promotion begins one: `promote_crossfade` overwrites
/// the governor's totals where they stand, from one track's length straight to the next.
/// Renewal keyed on those totals returning to zero was reachable only from a load, which
/// left the incoming track carrying an allowance the outgoing one had already spent, and
/// its own next fade with nothing to stage.
#[test]
fn a_crossfade_promotion_renews_the_head_allowance() {
    let bp = playing_with_a_collapsed_lead(100_000, 10_000_000);
    let mut governor = GovernorState::new();
    governor.gate = PreloadGate::Paused;
    governor.tick(&bp);
    // What the outgoing track drew through its own shut gate: six seconds at its bitrate.
    governor.preload_head_granted = bp.head_allowance();

    // The promotion, as `promote_crossfade` performs it: the totals are rewritten in
    // place, non-zero to non-zero, and the lead the fade left behind is gone by mid-track.
    bp.total_len.store(8_000_000, Relaxed);
    bp.written.store(50_000, Relaxed);
    bp.read_pos.store(0, Relaxed);
    bp.begin_track();

    let (queue, mut rx) = queued_preload(16_000);
    governor.preload_queue = queue;
    governor.tick(&bp);

    assert_eq!(
        governor.gate,
        PreloadGate::Paused,
        "half a second of lead keeps the gate shut, which is what the allowance is for"
    );
    assert!(
        rx.try_recv().is_ok(),
        "the incoming track draws its own head through that shut gate"
    );
    assert_eq!(
        governor.preload_head_granted, 16_000,
        "charged against a fresh allowance, not the outgoing track's remainder"
    );
}

/// Without an announced track change nothing is renewed: the allowance is spent once per
/// track, and a track that merely keeps playing does not get a second one.
#[test]
fn the_head_allowance_is_not_renewed_while_one_track_plays_on() {
    let bp = playing_with_a_collapsed_lead(100_000, 10_000_000);
    let mut governor = GovernorState::new();
    governor.gate = PreloadGate::Paused;
    governor.tick(&bp);
    governor.preload_head_granted = bp.head_allowance();

    let (queue, mut rx) = queued_preload(16_000);
    governor.preload_queue = queue;
    governor.tick(&bp);

    assert_eq!(
        governor.preload_head_granted,
        bp.head_allowance(),
        "the same track keeps the allowance it spent"
    );
    assert!(rx.try_recv().is_err(), "so nothing more goes through");
}

/// Why the defect above went unheard: a promotion hands over a buffer the fade filled ahead
/// of the listener, and that lead reopens the gate. While it lasts the preload is served by
/// the uncapped queue and never consults the allowance at all. The spent allowance only
/// starts to bite once the promoted track's own download falls back under a second ahead.
#[test]
fn the_lead_a_promotion_hands_over_reopens_the_gate() {
    let bp = playing_with_a_collapsed_lead(100_000, 10_000_000);
    let mut governor = GovernorState::new();
    governor.gate = PreloadGate::Paused;
    governor.preload_head_granted = bp.head_allowance();

    // Seven seconds of the incoming track landed during the overlap.
    bp.written.store(700_000, Relaxed);
    bp.read_pos.store(0, Relaxed);

    let (queue, mut rx) = queued_preload(16_000);
    governor.preload_queue = queue;
    governor.tick(&bp);

    assert_eq!(
        governor.gate,
        PreloadGate::Active,
        "over two seconds ahead reopens it"
    );
    assert!(
        rx.try_recv().is_ok(),
        "an open gate serves the preload whatever the allowance says"
    );
}

/// A seek's cooldown does not withhold the fade's head, and the reason is measured rather than
/// preferred. Every serve branch already requires `boost_start.is_none()`, and a seek that has
/// to refetch raises exactly that boost: for the entire window the listener spends waiting on
/// audio, preload is at zero either way. The resume target lands in tens of milliseconds, the
/// boost runs for hundreds. The cooldown outlives the boost, and all it can withhold in that
/// tail is the staged track's head. That head is what arming asks for four times a second to
/// recover the fade the seek itself cancelled; withholding it costs about a second of fade
/// and buys nothing back.
#[test]
fn the_seek_cooldown_does_not_withhold_the_fades_head() {
    let bp = playing_with_a_collapsed_lead(100_000, 10_000_000);
    // Seven seconds of lead, past the gate's reopen threshold: the gate stays open. The
    // cooldown is the only thing under test here.
    bp.written.store(700_000, Relaxed);
    bp.request_seek_preload_pause();

    let mut governor = GovernorState::new();
    let (queue, mut rx) = queued_preload(16_000);
    governor.preload_queue = queue;

    governor.tick(&bp);

    assert_eq!(
        governor.gate,
        PreloadGate::Active,
        "the lead is past the reopen threshold, so a shut gate is not what is being measured"
    );
    assert!(
        governor.is_preload_cooldown_active(),
        "the tick armed the cooldown the seek asked for"
    );
    assert!(
        rx.try_recv().is_ok(),
        "the cooldown withheld the head arming needs, and a fade the seek cancelled cannot \
         come back without it"
    );
}
