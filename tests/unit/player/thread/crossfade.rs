//! Tests for `src/player/thread/crossfade.rs`, attached to it by `#[path]`. The player under
//! test comes from the `harness` fixture the sidecars of `thread/` share.

use super::IncomingFailure;
use crate::player::thread::DecodeCommand;
use crate::player::thread::harness::{Harness, harness};
use crate::player::{LoadRequest, PlaybackState, PlayerEvent, ResumePolicy};
use crate::state::current_track_fixture::CurrentTrackSlot;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

/// What the incoming track has played at a promotion, in samples.
const SIX_SECONDS: u64 = 44_100 * 2 * 6;

/// A player mid-fade: the outgoing track is playing and the incoming one is armed.
/// The returned receiver keeps the incoming decoder's command channel alive.
fn arm_for_promotion(h: &mut Harness, incoming_secs: f64) -> mpsc::Receiver<DecodeCommand> {
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let (_event_tx, event_rx) = mpsc::channel();
    h.player.current_duration = 200.0;
    h.player.sample_rate = 44_100;
    h.player.channels = 2;
    // Where the outgoing track had got to: a large cumulative count, which is
    // exactly what must NOT become the incoming track's position.
    h.player.played_samples.store(44_100 * 2 * 195, Relaxed);
    h.player.pending_crossfade = Some(crate::player::thread::CrossfadeState {
        cmd_tx,
        event_rx,
        handle: None,
        reader_cancel: Arc::new(AtomicBool::new(false)),
        decoded: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        next: crate::state::RetainedTrack {
            track: crate::state::TrackInfo {
                url: "https://example.invalid/incoming".to_string(),
                key: String::new(),
                format: "flac".to_string(),
                product_id: Some("incoming-product".to_string()),
            },
            load_gen: crate::player::current_gen(),
        },
        // What `arm_crossfade` reads before validating the staged record.
        armed_over: crate::state::CURRENT_TRACK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .publication(),
        buffer: crate::player::buffer::RamBuffer::from_complete(vec![0u8; 64]),
        duration: incoming_secs,
        media_format: None,
        incoming_finished: false,
    });
    cmd_rx
}

/// A promotion is not a load: it publishes the generation its fade was ARMED under, carried on
/// the fade's own state rather than read from the counter at promotion time.
///
/// Re-reading it there races the bump on the IPC thread: a load starting during the overlap
/// would stamp ITS generation onto the track this promotion makes current, and the flush-side
/// replay guard tests exactly that stamp, replaying the wrong track and aborting the newer load.
#[test]
fn a_promotion_publishes_the_generation_its_fade_was_armed_under() {
    let dir = tempfile::tempdir().unwrap();
    let _slot = CurrentTrackSlot::clear();
    let mut h = harness(dir.path());
    let _incoming_cmds = arm_for_promotion(&mut h, 246.0);

    // A generation this test owns: a concurrent test moving the process-wide counter cannot
    // decide the outcome. Deliberately not the counter's current value: the gap between the two
    // is the entire difference between carrying the stamp and re-reading it.
    let armed_gen = crate::player::current_gen().wrapping_add(1_000);
    h.player
        .pending_crossfade
        .as_mut()
        .expect("the fade is armed")
        .next
        .load_gen = armed_gen;

    h.player.promote_crossfade(SIX_SECONDS);

    let retained = crate::state::CURRENT_TRACK
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retained()
        .cloned()
        .expect("a promotion publishes the incoming track");
    assert_eq!(
        retained.track.url, "https://example.invalid/incoming",
        "the promotion published the wrong track"
    );
    assert_eq!(
        retained.load_gen, armed_gen,
        "the promoted track carries a generation that is not its own, so a replay guard reading \
         it cannot tell this track from the load that superseded it"
    );
}

/// TIDAL's `handleAutomaticTransitionToPreloadedMediaProduct` awaits `mediaduration` and then
/// `mediastate: active` before it moves its own UI, so `completed` alone makes it enter that
/// handler and wait forever while every surface keeps naming the outgoing track. The duration
/// looks redundant because it drives no OS metadata; deleting it on that reasoning is the
/// regression this pins.
#[test]
fn a_promotion_publishes_the_three_events_the_sdk_waits_on() {
    let dir = tempfile::tempdir().unwrap();
    let _slot = CurrentTrackSlot::clear();
    let mut h = harness(dir.path());
    let _incoming_cmds = arm_for_promotion(&mut h, 246.0);

    h.player.promote_crossfade(SIX_SECONDS);

    let events = h.events.lock().unwrap();
    let seen: Vec<&PlayerEvent> = events
        .iter()
        .filter(|ev| {
            matches!(
                ev,
                PlayerEvent::CrossfadePromoted(..)
                    | PlayerEvent::Duration(..)
                    | PlayerEvent::StateChange(..)
            )
        })
        .collect();

    assert!(
        matches!(seen.first(), Some(PlayerEvent::CrossfadePromoted(..))),
        "the promotion announces itself first, got {seen:?}"
    );
    match seen.get(1) {
        Some(PlayerEvent::Duration(secs, _, id)) => {
            assert!(
                (*secs - 246.0).abs() < 1e-6,
                "the duration must be the INCOMING track's, got {secs}"
            );
            assert_eq!(id.as_deref(), Some("incoming-product"));
        }
        other => panic!("the SDK awaits mediaduration second, got {other:?}"),
    }
    assert!(
        matches!(
            seen.get(2),
            Some(PlayerEvent::StateChange(PlaybackState::Active, _))
        ),
        "the SDK awaits mediastate:active third, got {:?}",
        seen.get(2)
    );
}

/// The SDK's automatic-transition handler is the one transition that never assigns its own
/// `currentTime`. The position the promotion reports IS the SDK's position, and it has to be
/// EMITTED before the three events: emitted first it is already pending when `completed` forces
/// the flush, instead of waiting out a 24 ms debounce and a 250 ms throttle. Emission order is
/// not batch order, `take_flush_batch` appending the position BEHIND the queued events, which is
/// the order the SDK needs. This test pins the emission order only.
#[test]
fn a_promotion_emits_the_incoming_position_before_the_transition_events() {
    let dir = tempfile::tempdir().unwrap();
    let _slot = CurrentTrackSlot::clear();
    let mut h = harness(dir.path());
    let _incoming_cmds = arm_for_promotion(&mut h, 246.0);

    // Six seconds of the incoming track were drained during the overlap, and the
    // swapping tick left that on the clock. Priming it is what the position report
    // reads: the promotion no longer writes the clock, and without this the fixture's
    // outgoing total is what would be reported.
    h.player.played_samples.store(SIX_SECONDS, Relaxed);
    h.player.promote_crossfade(SIX_SECONDS);

    let events = h.events.lock().unwrap();
    let promoted = events
        .iter()
        .position(|ev| matches!(ev, PlayerEvent::CrossfadePromoted(..)))
        .expect("the promotion must announce itself");
    let time = events
        .iter()
        .position(|ev| matches!(ev, PlayerEvent::TimeUpdate(..)))
        .expect("the promotion must report the incoming track's position");
    assert!(
        time < promoted,
        "the position has to lead the transition, got {events:?}"
    );
    match &events[time] {
        PlayerEvent::TimeUpdate(secs, _) => assert!(
            (*secs - 6.0).abs() < 1e-6,
            "the position must be the INCOMING track's overlap drain, got {secs}"
        ),
        other => panic!("expected a position report, got {other:?}"),
    }
}

/// Arming is not the transition. A skip, a seek, a device switch or the incoming decoder failing
/// all cancel a fade, and the outgoing track then plays to its real end, where the completion
/// path looks for the staged next track: consumed at arm time it was already gone, and playback
/// stopped dead with the queue intact. It is spent at promotion instead.
///
/// This pins the second half only. That the ARMING no longer spends it cannot be driven from
/// here, `arm_crossfade` needing a real staged preload and an open device.
#[tokio::test]
async fn a_cancelled_fade_leaves_the_next_track_staged_and_a_promotion_spends_it() {
    let _serialised = crate::audio::preload::tests::PRELOAD_TESTS.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let _slot = CurrentTrackSlot::clear();
    let mut h = harness(dir.path());
    let _incoming_cmds = arm_for_promotion(&mut h, 246.0);
    let staged = crate::state::TrackInfo {
        url: "https://example.invalid/incoming".to_string(),
        key: String::new(),
        format: "flac".to_string(),
        product_id: Some("incoming-product".to_string()),
    };
    crate::audio::preload::PRELOAD_STATE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .stage_for_test(staged.clone());

    h.player.cancel_crossfade();

    assert!(
        crate::audio::preload::PRELOAD_STATE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .staged_next()
            .is_some(),
        "a cancelled fade took the queue's next track with it: the hard cut has nothing to advance to"
    );

    // The same fade, this time carried to its promotion.
    let mut h = harness(dir.path());
    let _incoming_cmds = arm_for_promotion(&mut h, 246.0);
    h.player.promote_crossfade(SIX_SECONDS);

    assert!(
        crate::audio::preload::PRELOAD_STATE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .staged_next()
            .is_none(),
        "the promoted track is current now, so its staged record must be spent"
    );
}

/// A fade the callback has already finished cannot be cancelled. It swaps its consumer and
/// stamps `done` inside one audio tick, and the player thread reads that stamp in its poll,
/// which runs AFTER the commands of the same pass. A stop, a seek or a skip landing in that
/// window retired the incoming decoder feeding the ring the device had just been handed:
/// silence with no producer left, no watchdog behind it, and a UI still naming the outgoing
/// track. The helper stands in for the callback: it holds the offer guard, publishes `done`,
/// and only then releases.
#[test]
fn a_teardown_waits_out_a_swap_in_progress_and_promotes_it() {
    let dir = tempfile::tempdir().unwrap();
    let _slot = CurrentTrackSlot::clear();
    let mut h = harness(dir.path());
    let incoming_cmds = arm_for_promotion(&mut h, 246.0);
    let link = crate::player::thread::output::CrossfadeLink {
        attach: Arc::new(Mutex::new(None)),
        cancel: Arc::new(AtomicBool::new(false)),
        out_eof: Arc::new(AtomicBool::new(false)),
        in_eof: Arc::new(AtomicBool::new(false)),
        done: Arc::new(std::sync::atomic::AtomicU64::new(0)),
    };
    h.player.cpal_xfade = Some(link.clone());

    let (held_tx, held_rx) = std::sync::mpsc::channel();
    let swapper = std::thread::spawn(move || {
        let _guard = link.attach.lock().unwrap();
        held_tx.send(()).unwrap();
        // Long enough for a teardown reading outside the guard to read first, which is how this
        // catches one. Under the guard the order is fixed however long this waits.
        std::thread::sleep(std::time::Duration::from_millis(50));
        link.done.store(
            crate::player::thread::output::pack_xfade_done(1, SIX_SECONDS),
            std::sync::atomic::Ordering::Release,
        );
    });
    held_rx.recv().unwrap();

    h.player.cancel_crossfade();
    swapper.join().unwrap();

    let events = h.events.lock().unwrap();
    assert!(
        events
            .iter()
            .any(|ev| matches!(ev, PlayerEvent::CrossfadePromoted(..))),
        "a swap in progress must promote, not cancel, got {events:?}"
    );
    assert!(
        !incoming_cmds
            .try_iter()
            .any(|cmd| matches!(cmd, DecodeCommand::Stop)),
        "the decoder feeding the promoted ring was stopped"
    );
}

/// A Load the generation gate discards must leave a live fade alone. Tearing it down first
/// retracts the attach slot and stops the incoming decoder, and nothing legitimate follows to
/// justify it: a generation can be minted and then die to an HTTP error (`ctx.fail_load`), and
/// `LoadFailed` does not cancel a fade. The discarded Load is then the sole cause of the loss,
/// and the outgoing gain snaps back mid-ramp.
#[tokio::test]
async fn a_stale_load_leaves_a_live_fade_armed() {
    // Held for the reason its siblings state: `handle_load` reads the process-wide generation.
    let _serialised = crate::audio::preload::tests::PRELOAD_TESTS.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());
    let _incoming_cmds = arm_for_promotion(&mut h, 246.0);

    // A generation already retired. Mints only move the counter forward; this one stays
    // stale whatever else the suite does between here and the gate.
    let superseded = crate::player::current_gen().wrapping_sub(1);

    let delivered = h.player.handle_load(
        LoadRequest {
            buffer: crate::player::buffer::RamBuffer::from_complete(vec![1, 2, 3]),
            load_gen: superseded,
            seq: 11,
            track_id: "track-superseded".to_string(),
            product_id: Some("superseded-product".to_string()),
            resume_policy: ResumePolicy::Restart,
            load_start: std::time::Instant::now(),
            cached: Some(crate::player::cache::Generation::NONE),
            format: "flac".to_string(),
        },
        false,
    );

    assert!(!delivered, "a stale Load must not deliver a track");
    assert!(
        h.player.pending_crossfade.is_some(),
        "a Load the gate discards tore the fade down on its way out"
    );
}

/// Arming polls four times a second for the tail of every track, each tick asking the cache
/// whether the next track is on disk. That mutex's holders (a store's row, a step of an eviction
/// or a Clear) can stall behind the disk: waiting here parks the control thread as long.
#[test]
fn a_busy_cache_never_parks_the_arming_tick() {
    let dir = tempfile::tempdir().unwrap();
    let h = harness(dir.path());
    let track = crate::state::TrackInfo {
        url: "https://example.invalid/cached.flac".to_string(),
        key: String::new(),
        format: "flac".to_string(),
        product_id: None,
    };

    // What a wipe looks like from here: the mutex held by someone else for a while.
    let (locked_tx, locked_rx) = mpsc::channel();
    let wiper = std::thread::spawn(move || {
        let _held = crate::state::AUDIO_CACHE
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        locked_tx.send(()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(400));
    });
    locked_rx.recv().unwrap();

    let asked_at = std::time::Instant::now();
    let answer = h.player.read_cached_track(&track);
    let waited = asked_at.elapsed();

    assert!(
        answer.is_none(),
        "a busy cache has not answered, and anything but a miss here would be a guess"
    );
    assert!(
        waited < std::time::Duration::from_millis(100),
        "the arming tick waited {waited:?} on the wipe: that is the transport frozen"
    );
    wiper.join().unwrap();
}

/// What the callback leaves behind when it completes a fade on its own clock.
fn finished_fade_link() -> crate::player::thread::output::CrossfadeLink {
    crate::player::thread::output::CrossfadeLink {
        attach: Arc::new(Mutex::new(None)),
        cancel: Arc::new(AtomicBool::new(false)),
        out_eof: Arc::new(AtomicBool::new(false)),
        in_eof: Arc::new(AtomicBool::new(false)),
        done: Arc::new(std::sync::atomic::AtomicU64::new(
            crate::player::thread::output::pack_xfade_done(1, SIX_SECONDS),
        )),
    }
}

/// The listener changing their mind and the incoming track dying are two different
/// cancellations, and a fade the callback has already finished is where they part. The promotion
/// is forced either way, but here the track promoted is the one whose decode thread has just
/// returned. Nothing else can ever report it: `Finished` never comes from a thread that exited,
/// and the drain the completion path waits on never arrives. Left unsettled, the UI names that
/// track Active over permanent silence, with no error and no skip.
#[test]
fn a_promoted_fade_settles_the_incoming_track_that_died() {
    let dir = tempfile::tempdir().unwrap();
    let _slot = CurrentTrackSlot::clear();
    let mut h = harness(dir.path());
    let _incoming_cmds = arm_for_promotion(&mut h, 246.0);
    h.player.cpal_xfade = Some(finished_fade_link());

    let must_settle = h
        .player
        .cancel_crossfade_after_incoming_failure(IncomingFailure::NetworkStalled);

    assert!(
        must_settle,
        "the fade promoted the track that just died, so the caller has to settle it"
    );
    assert!(
        h.player.decode_cmd_tx.is_none(),
        "the channel behind the promoted track is dead: left Some, a later seek arms a guard nothing can clear"
    );
    assert!(
        h.player
            .committed_track
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_none(),
        "a committed track names a decoder that has exited"
    );
    {
        let events = h.events.lock().unwrap();
        assert!(
            events
                .iter()
                .any(|ev| matches!(ev, PlayerEvent::NetworkLost(_))),
            "a stalled incoming track that got promoted must still report the loss, got {events:?}"
        );
    }

    // The same forced promotion, reached by a listener changing their mind instead. The
    // staged bytes are good: there is nothing to settle and nothing to report.
    let mut h = harness(dir.path());
    let _incoming_cmds = arm_for_promotion(&mut h, 246.0);
    h.player.cpal_xfade = Some(finished_fade_link());

    h.player.cancel_crossfade();

    let events = h.events.lock().unwrap();
    assert!(
        !events
            .iter()
            .any(|ev| matches!(ev, PlayerEvent::NetworkLost(_))),
        "a skip is not a failure: promoting for one must report nothing, got {events:?}"
    );
}

/// The outgoing counter stopped 2.0s short of the end while a fade was running, the drain check
/// read that as a finished track, and the teardown dropped the cpal stream BOTH tracks played
/// through. The counter cannot describe a fade (`fade_pos` advances on the callback's clock,
/// `played_samples` only on what the outgoing ring supplied): a fade is left to its own promotion.
#[test]
fn a_stalled_counter_does_not_complete_a_track_that_is_still_fading() {
    let dir = tempfile::tempdir().unwrap();
    let _slot = CurrentTrackSlot::clear();
    let mut h = harness(dir.path());
    let _incoming_cmds = arm_for_promotion(&mut h, 246.0);
    // The outgoing decoder has finished and the counter has stopped moving: exactly
    // the state the drain check completes a track on.
    h.player.pending_complete = true;
    h.player.last_played_snapshot = h.player.played_samples.load(Relaxed);

    h.player.poll_playback();

    assert!(
        h.player.pending_crossfade.is_some(),
        "the fade was torn down by the outgoing track's counter"
    );
    assert!(h.player.has_track, "the pipeline was dismantled mid-fade");
    let events = h.events.lock().unwrap();
    assert!(
        !events
            .iter()
            .any(|ev| matches!(ev, PlayerEvent::StateChange(PlaybackState::Completed, _))),
        "the track was announced completed while its fade was still running: {events:?}"
    );
}

/// A swap only happens once the outgoing decoder has reported `Finished`, which also sets
/// `pending_complete`. Carried onto the incoming track it suppresses every resume-store write for
/// it and turns two stalled poll ticks into a false completion. The clock is the other half and
/// NOT this thread's to move: the callback rebased it in the tick that swapped and has credited
/// the promoted ring since, so a store here can only throw that interval away. Both halves are
/// checked against the state a real fade leaves behind.
#[test]
fn a_promotion_keeps_the_clock_the_swap_set_and_clears_the_completion_state() {
    let dir = tempfile::tempdir().unwrap();
    let _slot = CurrentTrackSlot::clear();
    let mut h = harness(dir.path());
    let _incoming_cmds = arm_for_promotion(&mut h, 246.0);
    // What a real fade leaves behind: the outgoing track finished.
    h.player.pending_complete = true;
    h.player.last_played_snapshot = 44_100 * 2 * 195;

    // And what the callback leaves behind: the base it set at the swap, with one
    // poll's worth of the promoted ring already on top. The fixture's outgoing total
    // is overwritten here exactly as the swapping tick overwrites it.
    let in_played = SIX_SECONDS;
    let after_swap = 44_100 * 2;
    h.player
        .played_samples
        .store(in_played + after_swap, Relaxed);

    h.player.promote_crossfade(in_played);

    assert_eq!(
        h.player.played_samples.load(Relaxed),
        in_played + after_swap,
        "the promotion moved a clock it does not own, discarding the second of audio \
         the callback played between the swap and this poll"
    );
    assert!(
        (h.player.current_duration - 246.0).abs() < 1e-6,
        "the duration still described the outgoing track"
    );
    assert!(
        !h.player.pending_complete,
        "the outgoing track's completion was inherited by the incoming one"
    );
    assert_eq!(h.player.last_played_snapshot, 0);
    assert!(
        h.player.pending_crossfade.is_none(),
        "the fade must be retired by its own promotion"
    );
}

/// The two mirrors that describe the buffer being REPLACED, and that only `handle_load` used to
/// retire: `buffer_stalled` mirrors a flag living on the buffer object, read against the incoming
/// buffer by an edge detector still holding the outgoing value, and `xfade_refused_url` is a
/// refusal decided for the track the promotion just retired.
#[test]
fn a_promotion_retires_the_mirrors_that_described_the_outgoing_buffer() {
    let dir = tempfile::tempdir().unwrap();
    let _slot = CurrentTrackSlot::clear();
    let mut h = harness(dir.path());
    let _incoming_cmds = arm_for_promotion(&mut h, 246.0);
    h.player.buffer_stalled = true;
    h.player.xfade_refused_url = Some("https://example.invalid/refused".to_string());

    h.player.promote_crossfade(SIX_SECONDS);

    assert!(
        !h.player.buffer_stalled,
        "the stall mirror still described the outgoing buffer, which the swap replaced"
    );
    assert!(
        h.player.xfade_refused_url.is_none(),
        "the promoted track inherited a refusal decided for the track it replaced"
    );
}

/// The badge reported the file's rate while the stream could be running at another, already true
/// before the engine-rate change and simply invisible. The snapshot now carries both.
///
/// Coverage note: this hand-builds the snapshot and injects it into
/// `pending_crossfade.media_format`, proving only that `promote_crossfade` forwards both fields
/// without swapping them. `handle_load`'s own construction site opens a real device and cannot
/// run in this harness.
#[test]
fn a_promotion_reports_both_the_file_rate_and_the_output_rate() {
    let dir = tempfile::tempdir().unwrap();
    let _slot = CurrentTrackSlot::clear();
    let mut h = harness(dir.path());
    let _incoming_cmds = arm_for_promotion(&mut h, 246.0);
    h.player.sample_rate = 192_000;
    h.player.pending_crossfade.as_mut().unwrap().media_format =
        Some(crate::player::MediaFormatSnapshot {
            codec: "flac",
            sample_rate: 96_000,
            output_sample_rate: 192_000,
            bit_depth: Some(24),
            channels: 2,
            bytes: 100,
        });

    h.player.promote_crossfade(SIX_SECONDS);

    let events = h.events.lock().unwrap();
    let found = events.iter().any(|ev| {
        matches!(
            ev,
            PlayerEvent::MediaFormat {
                sample_rate: 96_000,
                output_sample_rate: 192_000,
                ..
            }
        )
    });
    assert!(found, "both rates must reach the renderer, got {events:?}");
}

const SKIPPED_TO_URL: &str = "https://example.invalid/skipped-to?sig=fresh";
const SKIPPED_TO_ID: &str = "https://example.invalid/skipped-to";
const INCOMING_URL: &str = "https://example.invalid/incoming";

fn a_track_at(url: &str) -> crate::state::TrackInfo {
    crate::state::TrackInfo {
        url: url.to_string(),
        key: String::new(),
        format: "flac".to_string(),
        product_id: Some("c-product".to_string()),
    }
}

fn publish_source(load_gen: u32, track: Option<crate::state::TrackInfo>) {
    crate::state::CURRENT_TRACK
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .publish_load(load_gen, track);
}

fn published_source() -> Option<crate::state::RetainedTrack> {
    crate::state::CURRENT_TRACK
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retained()
        .cloned()
}

/// The IPC thread's half of a load, long before the player thread sees it: `begin_load` mints the
/// generation, then `load_with_policy` mints the event seq and publishes.
fn load_c_on_the_ipc_thread() -> (u32, u32) {
    let gen_c = crate::player::begin_load(crate::player::LoadOrigin::Local);
    let seq_c = crate::player::next_event_seq();
    publish_source(gen_c, Some(a_track_at(SKIPPED_TO_URL)));
    (gen_c, seq_c)
}

/// A link whose callback has already swapped the rings: the fade is finished, not yet promoted.
fn swapped_link() -> crate::player::thread::output::CrossfadeLink {
    crate::player::thread::output::CrossfadeLink {
        attach: Arc::new(Mutex::new(None)),
        cancel: Arc::new(AtomicBool::new(false)),
        out_eof: Arc::new(AtomicBool::new(false)),
        in_eof: Arc::new(AtomicBool::new(false)),
        done: Arc::new(std::sync::atomic::AtomicU64::new(
            crate::player::thread::output::pack_xfade_done(1, SIX_SECONDS),
        )),
    }
}

fn handle_load_of_c(h: &mut Harness, gen_c: u32, seq_c: u32) -> bool {
    h.player.handle_load(
        LoadRequest {
            buffer: crate::player::buffer::RamBuffer::from_complete(vec![1, 2, 3]),
            load_gen: gen_c,
            seq: seq_c,
            track_id: SKIPPED_TO_ID.to_string(),
            product_id: Some("c-product".to_string()),
            resume_policy: ResumePolicy::Restart,
            load_start: std::time::Instant::now(),
            cached: Some(crate::player::cache::Generation::NONE),
            format: "flac".to_string(),
        },
        false,
    )
}

fn assert_the_slot_names_c(gen_c: u32) {
    let slot = published_source().expect("the slot is empty");
    assert_eq!(
        slot.track.url, SKIPPED_TO_URL,
        "CURRENT_TRACK no longer names C"
    );
    assert_eq!(
        slot.load_gen, gen_c,
        "CURRENT_TRACK carries a stale generation"
    );
}

/// The seqs the renderer's bridge orders by: it drops anything below the highest it has seen.
fn max_emitted_seq(h: &Harness) -> Option<u32> {
    h.events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|ev| match ev {
            PlayerEvent::TimeUpdate(_, seq)
            | PlayerEvent::StateChange(_, seq)
            | PlayerEvent::CrossfadePromoted(seq, _)
            | PlayerEvent::Duration(_, seq, _) => Some(*seq),
            _ => None,
        })
        .max()
}

/// The swap lands between C's publication and `handle_load(C)`; the poll promotes first.
#[tokio::test]
async fn a_promotion_yields_the_slot_to_a_load_published_after_its_fade_armed() {
    let _serialised = crate::audio::preload::tests::PRELOAD_TESTS.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let _slot = CurrentTrackSlot::clear();
    let mut h = harness(dir.path());
    let _incoming = arm_for_promotion(&mut h, 246.0);
    h.player.cpal_xfade = Some(swapped_link());

    let (gen_c, seq_c) = load_c_on_the_ipc_thread();
    h.player.handle_load_started(gen_c);
    assert!(h.player.reconcile_completed_crossfade());
    handle_load_of_c(&mut h, gen_c, seq_c);
    assert_the_slot_names_c(gen_c);
}

/// A stop mints a generation and publishes nothing: the promotion is still the newest source.
#[tokio::test]
async fn a_stop_during_the_fade_leaves_the_promotion_publishing() {
    let _serialised = crate::audio::preload::tests::PRELOAD_TESTS.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let _slot = CurrentTrackSlot::clear();
    let gen_a = crate::player::begin_load(crate::player::LoadOrigin::Local);
    publish_source(
        gen_a,
        Some(a_track_at("https://example.invalid/outgoing?sig=a")),
    );
    let mut h = harness(dir.path());
    let _incoming = arm_for_promotion(&mut h, 246.0);

    let _gen_stop = crate::player::begin_load(crate::player::LoadOrigin::Local);
    h.player.promote_crossfade(SIX_SECONDS);

    let slot = published_source().expect("the slot is empty");
    assert_eq!(
        slot.track.url, INCOMING_URL,
        "a stop's mint suppressed the promotion's publish"
    );
}

/// The staged record carries the stop's generation and the slot the outgoing load's: they differ,
/// and still nothing newer was published.
#[tokio::test]
async fn a_stop_before_arming_leaves_the_promotion_publishing() {
    let _serialised = crate::audio::preload::tests::PRELOAD_TESTS.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let _slot = CurrentTrackSlot::clear();
    let gen_a = crate::player::begin_load(crate::player::LoadOrigin::Local);
    publish_source(
        gen_a,
        Some(a_track_at("https://example.invalid/outgoing?sig=a")),
    );
    let gen_stop = crate::player::begin_load(crate::player::LoadOrigin::Local);
    let mut h = harness(dir.path());
    let _incoming = arm_for_promotion(&mut h, 246.0);
    assert_eq!(
        h.player.pending_crossfade.as_ref().unwrap().next.load_gen,
        gen_stop
    );

    h.player.promote_crossfade(SIX_SECONDS);

    let slot = published_source().expect("the slot is empty");
    assert_eq!(
        slot.track.url, INCOMING_URL,
        "the promotion yielded to a stop"
    );
    assert_eq!(slot.load_gen, gen_stop);
}

#[tokio::test]
async fn a_second_absence_is_newer_than_the_first() {
    let _serialised = crate::audio::preload::tests::PRELOAD_TESTS.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let _slot = CurrentTrackSlot::clear();
    let gen_d1 = crate::player::begin_load(crate::player::LoadOrigin::Local);
    publish_source(gen_d1, None);
    let mut h = harness(dir.path());
    let _incoming = arm_for_promotion(&mut h, 246.0);

    let gen_d2 = crate::player::begin_load(crate::player::LoadOrigin::Local);
    publish_source(gen_d2, None);
    h.player.promote_crossfade(SIX_SECONDS);

    assert!(
        published_source().is_none(),
        "the promotion overwrote the newer DASH load's absence with {:?}",
        published_source()
    );
}

#[tokio::test]
async fn a_fade_out_of_a_dash_track_publishes_the_promoted_one() {
    let _serialised = crate::audio::preload::tests::PRELOAD_TESTS.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let _slot = CurrentTrackSlot::clear();
    let gen_d1 = crate::player::begin_load(crate::player::LoadOrigin::Local);
    publish_source(gen_d1, None);
    let mut h = harness(dir.path());
    let _incoming = arm_for_promotion(&mut h, 246.0);

    h.player.promote_crossfade(SIX_SECONDS);

    let slot = published_source().expect("the promotion out of DASH published nothing");
    assert_eq!(slot.track.url, INCOMING_URL);
}

/// A restart of the outgoing track is a newer load of the SAME id: the track cannot tell it
/// apart, the publication can.
#[tokio::test]
async fn a_restart_of_the_outgoing_track_during_the_fade_keeps_its_publication() {
    let _serialised = crate::audio::preload::tests::PRELOAD_TESTS.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let _slot = CurrentTrackSlot::clear();
    let gen_a = crate::player::begin_load(crate::player::LoadOrigin::Local);
    publish_source(
        gen_a,
        Some(a_track_at("https://example.invalid/outgoing?sig=a")),
    );
    let mut h = harness(dir.path());
    let _incoming = arm_for_promotion(&mut h, 246.0);

    let gen_r = crate::player::begin_load(crate::player::LoadOrigin::Local);
    publish_source(
        gen_r,
        Some(a_track_at("https://example.invalid/outgoing?sig=r")),
    );
    h.player.promote_crossfade(SIX_SECONDS);

    let slot = published_source().expect("the slot is empty");
    assert_eq!(slot.track.url, "https://example.invalid/outgoing?sig=r");
    assert_eq!(slot.load_gen, gen_r);
}

/// The two mints of a load through `load_with_policy`, its publication left out: the generation
/// a `begin_load` caller hands it, then the event seq it takes from `next_event_seq`.
fn mint_a_load_on_the_ipc_thread() -> (u32, u32) {
    let load_gen = crate::player::begin_load(crate::player::LoadOrigin::Local);
    (load_gen, crate::player::next_event_seq())
}

/// A track playing under the seq its own load minted, with a fade to B finished by the callback
/// and not yet adopted.
fn a_finished_fade_out_of_a_loaded_track(h: &mut Harness) -> mpsc::Receiver<DecodeCommand> {
    let (gen_a, seq_a) = mint_a_load_on_the_ipc_thread();
    publish_source(
        gen_a,
        Some(a_track_at("https://example.invalid/outgoing?sig=a")),
    );
    h.player.current_seq = seq_a;
    let incoming = arm_for_promotion(h, 246.0);
    h.player.cpal_xfade = Some(swapped_link());
    incoming
}

/// A stop landing between the swap and the next poll promotes inside its own teardown, announced
/// but never above the stop's seq: the bridge would drop `Paused`, `Stopped` and every event of B.
#[tokio::test]
async fn a_stop_whose_teardown_promotes_still_settles_on_stopped() {
    let _serialised = crate::audio::preload::tests::PRELOAD_TESTS.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let _slot = CurrentTrackSlot::clear();
    let mut h = harness(dir.path());
    let _incoming = a_finished_fade_out_of_a_loaded_track(&mut h);

    // `Player::stop`: a generation and a seq, no publication.
    let (_gen_stop, seq_stop) = mint_a_load_on_the_ipc_thread();
    h.player.handle_stop(seq_stop);

    let events = h.events.lock().unwrap().clone();
    assert!(
        events
            .iter()
            .any(|ev| matches!(ev, PlayerEvent::CrossfadePromoted(..))),
        "the finished fade must still be announced, got {events:?}"
    );
    let max = max_emitted_seq(&h);
    assert!(
        max.is_none_or(|m| m <= seq_stop),
        "an event carrying seq {max:?} outranks the stop's seq {seq_stop}: the bridge drops Stopped, got {events:?}"
    );
    assert_eq!(h.player.current_seq, seq_stop);
}

/// `load_with_policy` mints C's seq, then works for a while before it publishes C. A poll landing
/// in between sees nothing newer published and announces the promotion; under a seq of its own it
/// would outrank C's, and the bridge would drop every event C emits.
#[tokio::test]
async fn a_promotion_before_a_load_publishes_does_not_outrank_its_seq() {
    let _serialised = crate::audio::preload::tests::PRELOAD_TESTS.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let _slot = CurrentTrackSlot::clear();
    let mut h = harness(dir.path());
    let _incoming = a_finished_fade_out_of_a_loaded_track(&mut h);

    let (gen_c, seq_c) = mint_a_load_on_the_ipc_thread();
    assert!(h.player.reconcile_completed_crossfade());
    publish_source(gen_c, Some(a_track_at(SKIPPED_TO_URL)));
    h.player.handle_load_started(gen_c);
    handle_load_of_c(&mut h, gen_c, seq_c);

    let max = max_emitted_seq(&h);
    assert_eq!(h.player.current_seq, seq_c);
    assert!(
        max.is_none_or(|m| m <= seq_c),
        "an event carrying seq {max:?} outranks C's seq {seq_c}: the bridge drops every event C emits"
    );
}

/// A promotion mints no seq: only the missing `completed` tells a superseded one apart.
#[tokio::test]
async fn a_superseded_promotion_announces_no_transition() {
    let _serialised = crate::audio::preload::tests::PRELOAD_TESTS.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let _slot = CurrentTrackSlot::clear();
    let mut h = harness(dir.path());
    let _incoming = arm_for_promotion(&mut h, 246.0);
    h.player.cpal_xfade = Some(swapped_link());

    let (gen_c, _seq_c) = load_c_on_the_ipc_thread();
    h.player.handle_load_started(gen_c);
    assert!(h.player.reconcile_completed_crossfade());

    let events = h.events.lock().unwrap();
    assert!(
        !events
            .iter()
            .any(|ev| matches!(ev, PlayerEvent::CrossfadePromoted(..))),
        "a superseded promotion announced its transition, got {events:?}"
    );
}
