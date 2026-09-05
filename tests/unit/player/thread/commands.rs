//! Tests for `src/player/thread/commands.rs`, attached to it by `#[path]`. The harness at
//! the bottom also drives `poll_playback`, which lives in the sibling `playback` module:
//! both are `pub(in player::thread)`: either file reaches the whole surface.

#[cfg(target_os = "windows")]
use super::queued_seek_survives;
use super::{
    IncomingFailure, LoadRequest, PlayAction, ResumePolicy, decide_play, resolve_start_position,
    settle_load,
};
#[cfg(target_os = "windows")]
use crate::player::asio::host::{AsioEvent, AsioHandle};
use crate::player::resume::ResumeStore;
use crate::player::thread::{DecodeCommand, DecodeEvent, PlayerThread};
use crate::player::{PlaybackState, PlayerCommand, PlayerEvent};
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

#[test]
fn play_with_live_track_resumes() {
    assert_eq!(decide_play(true, None, false), PlayAction::Resume);
    assert_eq!(decide_play(true, Some(7), true), PlayAction::Resume);
}

#[test]
fn play_while_loading_defers_to_that_generation() {
    assert_eq!(decide_play(false, Some(7), true), PlayAction::DeferTo(7));
    // a load in flight wins even when a retained source also exists
    assert_eq!(decide_play(false, Some(3), false), PlayAction::DeferTo(3));
}

#[test]
fn play_with_no_load_but_retained_source_rearms() {
    assert_eq!(decide_play(false, None, true), PlayAction::ReArm);
}

#[test]
fn play_cold_empty_is_ignored() {
    assert_eq!(decide_play(false, None, false), PlayAction::Ignore);
}

#[test]
fn settle_clears_loading_for_matching_gen() {
    assert_eq!(settle_load(Some(5), None, 5), (None, None));
}

#[test]
fn settle_ignores_a_mismatched_gen() {
    // a stale settle for an old gen must not clear a newer in-flight load
    assert_eq!(settle_load(Some(6), None, 5), (Some(6), None));
}

#[test]
fn settle_clears_a_deferred_play_waiting_on_a_failed_load() {
    // the load failed (no handle_load delivered); the deferred play
    // tagged with that gen must not dangle
    assert_eq!(settle_load(Some(5), Some(5), 5), (None, None));
}

#[test]
fn settle_leaves_a_deferred_play_for_a_different_gen() {
    assert_eq!(settle_load(None, Some(9), 5), (None, Some(9)));
}

/// The teardown used to clear the queued seek whatever track was loading next.
#[cfg(target_os = "windows")]
#[test]
fn a_queued_seek_survives_the_re_arm_of_its_own_track() {
    assert!(queued_seek_survives(
        Some("track-1"),
        "track-1",
        ResumePolicy::Explicit(12.0)
    ));
}

#[cfg(target_os = "windows")]
#[test]
fn a_queued_seek_does_not_follow_the_listener_to_another_track() {
    assert!(!queued_seek_survives(
        Some("track-1"),
        "track-2",
        ResumePolicy::Explicit(12.0)
    ));
}

#[cfg(target_os = "windows")]
#[test]
fn a_restart_discards_a_queued_seek_for_its_own_track() {
    // A fresh play instance contracts to start at 0, whatever the tag still holds.
    assert!(!queued_seek_survives(
        Some("track-1"),
        "track-1",
        ResumePolicy::Restart
    ));
}

#[cfg(target_os = "windows")]
#[test]
fn no_queued_seek_leaves_nothing_to_carry() {
    assert!(!queued_seek_survives(
        None,
        "track-1",
        ResumePolicy::Explicit(12.0)
    ));
}

/// The load's announcement and the stream it opens read the same pair a few lines apart.
/// Answering here, once, is what keeps the two from naming different positions.
#[test]
fn a_queued_seek_outranks_an_auto_resume() {
    assert_eq!(resolve_start_position(Some(10.0), Some(45.0)), Some(10.0));
    assert_eq!(resolve_start_position(None, Some(45.0)), Some(45.0));
    assert_eq!(resolve_start_position(Some(10.0), None), Some(10.0));
    assert_eq!(resolve_start_position(None, None), None);
}

// The transport methods are plain `&mut self` functions. A test drives them directly:
// no run loop, no audio device, no decode thread. The decode thread's reports arrive on a
// channel the test owns. `#[tokio::test]` throughout, since GOVERNOR's init calls tokio::spawn.

type Callback = Box<dyn Fn(PlayerEvent) + Send>;
type Spy = Arc<Mutex<Vec<PlayerEvent>>>;

struct Harness {
    player: PlayerThread<Callback>,
    events: Spy,
    /// Stands in for the decode thread's event sender.
    decode_events: mpsc::Sender<DecodeEvent>,
    /// Both are held for the test's lifetime, keeping the receivers the player owns from
    /// reporting a hung-up channel.
    _cmd_tx: mpsc::Sender<PlayerCommand>,
    _decode_cmds: mpsc::Receiver<DecodeCommand>,
}

/// A player holding one loaded, playing shared-path track. `dir` owns the resume file, so
/// no ordering or timing of the calls below can reach the one the running app writes.
fn harness(dir: &std::path::Path) -> Harness {
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let events: Spy = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let callback: Callback = Box::new(move |ev| sink.lock().unwrap().push(ev));
    let mut player = PlayerThread::new(cmd_rx, callback, false, Arc::new(Mutex::new(None)))
        .expect("construction opens no device and cannot fail off Windows");

    player.resume_store = ResumeStore::new(dir.join("resume_position.json"));

    let (decode_cmd_tx, decode_cmds) = mpsc::channel();
    let (decode_event_tx, decode_event_rx) = mpsc::channel();
    player.decode_cmd_tx = Some(decode_cmd_tx);
    player.decode_event_rx = Some(decode_event_rx);
    player.has_track = true;
    player.is_playing = true;
    player.current_track_id = Some("track-1".to_string());

    Harness {
        player,
        events,
        decode_events: decode_event_tx,
        _cmd_tx: cmd_tx,
        _decode_cmds: decode_cmds,
    }
}

/// The transport state announced last, ignoring position and format events.
fn last_state(events: &Spy) -> Option<PlaybackState> {
    events.lock().unwrap().iter().rev().find_map(|ev| match ev {
        PlayerEvent::StateChange(state, _) => Some(*state),
        _ => None,
    })
}

/// The track id stamped on the last announced length. The outer `Option` says whether a length
/// was announced at all, the inner one which track it was measured on.
fn last_duration_track(events: &Spy) -> Option<Option<String>> {
    events.lock().unwrap().iter().rev().find_map(|ev| match ev {
        PlayerEvent::Duration(_, _, id) => Some(id.clone()),
        _ => None,
    })
}

/// The chain the measured-length fix rests on: the id a load carries becomes the thread's, and
/// the thread's is what stamps every length it announces. Unprobeable bytes reach that
/// announcement without a decoder or a device, which is what keeps this test in a suite where
/// nothing opens an audio endpoint.
#[tokio::test]
async fn an_announced_length_names_the_track_its_load_carried() {
    // Held for the reason `load_state_tests` states: this samples the process-wide generation
    // and `handle_load` reads it again; a mint from another test landing between the two
    // makes the load read as stale and ignored.
    let _serialised = crate::audio::preload::tests::PRELOAD_TESTS.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());

    let delivered = h.player.handle_load(
        LoadRequest {
            buffer: crate::player::buffer::RamBuffer::from_complete(vec![1, 2, 3]),
            load_gen: crate::player::current_gen(),
            seq: 7,
            track_id: "track-1".to_string(),
            product_id: Some("120002099".to_string()),
            resume_policy: ResumePolicy::Restart,
            load_start: std::time::Instant::now(),
            cached: true,
            format: "flac".to_string(),
        },
        false,
    );

    assert!(!delivered, "three bytes cannot probe as audio");
    assert_eq!(
        h.player.current_product_id.as_deref(),
        Some("120002099"),
        "the load's own id never reached the thread"
    );
    assert_eq!(
        last_duration_track(&h.events),
        Some(Some("120002099".to_string())),
        "the announced length named a track other than the load it came from"
    );
}

/// A load that fails leaves nothing loaded. `committed_track` answers `Player::load`'s "same
/// track?" on the caller thread, but `decide_play` keys on `has_track`: leaving that true let a
/// re-assert minted before the failure resume a pipeline built from the buffer this load cancelled.
#[tokio::test]
async fn a_failed_load_leaves_no_track_behind() {
    // Same reason as its siblings, and this one hid the race well: a stale generation returns
    // false from the guard at the top of `handle_load`; the first assertion below still
    // passed (for a reason that has nothing to do with three bytes failing to probe), and
    // only the flag the deeper path would have cleared gave it away.
    let _serialised = crate::audio::preload::tests::PRELOAD_TESTS.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());
    assert!(h.player.has_track, "the harness starts on a live track");

    let delivered = h.player.handle_load(
        LoadRequest {
            buffer: crate::player::buffer::RamBuffer::from_complete(vec![1, 2, 3]),
            load_gen: crate::player::current_gen(),
            seq: 3,
            track_id: "track-2".to_string(),
            product_id: None,
            resume_policy: ResumePolicy::Restart,
            load_start: std::time::Instant::now(),
            cached: true,
            format: "flac".to_string(),
        },
        false,
    );

    assert!(!delivered, "three bytes cannot probe as audio");
    assert!(
        !h.player.has_track,
        "a failed load left a track flag that nothing backs"
    );
}

/// The re-assert is the one load path that never reaches `handle_load`, so it is the only one that
/// has to carry the id itself. Dropping it there left a gapless-advanced track nameless for the
/// rest of its life, with every length it measured publishable under nothing.
#[tokio::test]
async fn a_re_assert_brings_the_track_its_load_named() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());
    h.player.current_duration = 214.0;

    h.player.handle_command(PlayerCommand::ReassertResume {
        want_play: false,
        product_id: Some("120002099".to_string()),
        track_id: "track-1".to_string(),
    });

    assert_eq!(
        h.player.current_product_id.as_deref(),
        Some("120002099"),
        "the re-assert's own id never reached the thread"
    );
    assert_eq!(
        last_duration_track(&h.events),
        Some(Some("120002099".to_string())),
        "the re-asserted length named a track other than the load that carried it"
    );
}

/// A re-assert carrying no id refreshes nothing and erases nothing. A quality swap arrives that
/// way, and blanking the name there is what stopped the swap from ever republishing its length.
#[tokio::test]
async fn a_re_assert_without_an_id_keeps_the_one_already_known() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());
    h.player.current_duration = 214.0;
    h.player.current_product_id = Some("88264189".to_string());

    h.player.handle_command(PlayerCommand::ReassertResume {
        want_play: false,
        product_id: None,
        track_id: "track-1".to_string(),
    });

    assert_eq!(
        last_duration_track(&h.events),
        Some(Some("88264189".to_string())),
        "an id-less re-assert blanked the track's name"
    );
}

/// The re-assert is minted from a `committed_track` read on the caller's thread, so a load for
/// another track can commit between the mint and the handling. Applying the id then would rename
/// the arriving track after the one that just left, and every length it measures along with it.
#[tokio::test]
async fn a_re_assert_for_a_superseded_track_keeps_the_live_id() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());
    h.player.current_duration = 214.0;
    // What the thread committed to while the re-assert for `track-1` was still in flight.
    h.player.current_track_id = Some("track-2".to_string());
    h.player.current_product_id = Some("the-live-one".to_string());

    h.player.handle_command(PlayerCommand::ReassertResume {
        want_play: false,
        product_id: Some("the-superseded-one".to_string()),
        track_id: "track-1".to_string(),
    });

    assert_eq!(
        h.player.current_product_id.as_deref(),
        Some("the-live-one"),
        "a re-assert for a superseded track renamed the track that replaced it"
    );
    assert_eq!(
        last_duration_track(&h.events),
        Some(Some("the-live-one".to_string())),
        "the live track's length went out under the superseded track's name"
    );
}

use crate::state::current_track_fixture::CurrentTrackSlot;

/// A re-armable track in the process-wide slot, for one test. A device switch refuses a live
/// track it cannot replay.
///
/// A thin naming of [`CurrentTrackSlot::holding`]: the exclusion and the restore-on-drop come
/// from there, this only says which track a replay-authorising slot holds.
#[cfg(target_os = "windows")]
struct ReplayableTrack {
    // Named and underscored rather than a tuple field: the slot is held for its Drop, never
    // read, and `dead_code` refuses an unread tuple field.
    _slot: CurrentTrackSlot,
}

#[cfg(target_os = "windows")]
impl ReplayableTrack {
    fn set(url: &str) -> Self {
        Self {
            _slot: CurrentTrackSlot::holding(crate::state::RetainedTrack {
                track: crate::state::TrackInfo {
                    url: url.to_string(),
                    key: String::new(),
                    format: "flac".to_string(),
                    // Replayability turns on the credential and the format, never the id.
                    product_id: None,
                },
                // Stamped as a freshly published source would be: a replay this fixture
                // authorises is one the flush-side guard would also let through.
                load_gen: crate::player::current_gen(),
            }),
        }
    }
}

#[tokio::test]
async fn a_seek_taken_while_paused_settles_without_a_resume() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());

    h.player.handle_pause();
    h.player.handle_seek(30.0);
    assert!(
        h.player.seeking,
        "the seek is armed before the decoder answers"
    );

    h.decode_events
        .send(DecodeEvent::SeekComplete {
            gen_id: h.player.seek_ack_gen,
            position: 30.0,
            refused: false,
        })
        .unwrap();
    h.player.poll_playback();

    assert!(
        !h.player.seeking,
        "a paused poll has to read the decoder's answer; waiting for Play strands it"
    );
    assert_eq!(h.player.seek_target, None);
}

#[cfg(target_os = "windows")]
#[tokio::test]
async fn a_bypass_decode_failure_frees_the_device_and_keeps_the_resume_point() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());
    h.player.is_exclusive_mode = true;
    h.player.resume_store.set("track-1", 42.0);

    h.player
        .settle_bypass_decode_failure("decode packet error: truncated frame".to_string());

    assert!(
        h.player.exclusive_release_at.is_some(),
        "nothing else hands the device back: no EndStream means no completion path"
    );
    assert_eq!(
        h.player.resume_store.get("track-1"),
        Some(42.0),
        "clearing it here would be treating a dead decoder as a finished track"
    );
    assert!(!h.player.has_track);
    assert!(!h.player.is_playing);
    assert!(!h.player.seeking);
    assert!(
        h.player.exclusive_seek_tx.is_none(),
        "the sender points at a receiver the dead decoder dropped"
    );
    assert_eq!(h.player.idle_state, PlaybackState::Stopped);
    assert_eq!(last_state(&h.events), Some(PlaybackState::Stopped));
    assert!(
        h.events
            .lock()
            .unwrap()
            .iter()
            .any(|ev| matches!(ev, PlayerEvent::MediaError { .. })),
        "a terminal state with no error leaves the SDK without a reason"
    );
}

#[cfg(target_os = "windows")]
#[tokio::test]
async fn a_seek_with_no_live_asio_decoder_is_queued_against_the_current_track() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());
    h.player.is_asio_mode = true;
    h.player.has_track = false;

    h.player.handle_seek(30.0);

    assert_eq!(
        h.player.user_seek_override,
        Some(("track-1".to_string(), 30.0))
    );
    assert_eq!(
        h.player.take_user_seek_override(),
        Some(30.0),
        "the stream start reads the position back out from under the tag"
    );
}

/// A decode failure drops the track identity before the SDK's recovery reload. Queuing an
/// untagged seek there would hand it to whichever track loads next.
#[cfg(target_os = "windows")]
#[tokio::test]
async fn a_seek_with_no_track_to_tag_is_not_queued() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());
    h.player.is_asio_mode = true;
    h.player.has_track = false;
    h.player.current_track_id = None;

    h.player.handle_seek(30.0);

    assert!(h.player.user_seek_override.is_none());
}

/// Dropping the untagged seek is the deliberate half; the silence was not. With no tag the seek
/// is gone, so nothing will ever make the target the transport already took come true.
#[cfg(target_os = "windows")]
#[tokio::test]
async fn a_dropped_queued_seek_still_answers_with_the_backend_position() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());
    h.player.is_asio_mode = true;
    h.player.has_track = false;
    h.player.current_track_id = None;
    h.player.last_asio_pos = Some(12.5);

    h.player.handle_seek(30.0);

    assert!(
        h.player.user_seek_override.is_none(),
        "the answer replaces the silence, not the drop: an untagged seek is still discarded"
    );
    let positions: Vec<f64> = h
        .events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|ev| match ev {
            PlayerEvent::TimeUpdate(t, _) => Some(*t),
            _ => None,
        })
        .collect();
    assert_eq!(
        positions,
        vec![12.5],
        "the transport hears where the backend stands, not a target it will never reach"
    );
}

/// Taking only the winner leaves the loser armed for the next read of that slot.
#[cfg(target_os = "windows")]
#[tokio::test]
async fn taking_the_queued_seek_retires_the_auto_resume() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());
    h.player.user_seek_override = Some(("track-1".to_string(), 10.0));
    h.player.pending_resume_seek = Some(45.0);

    assert_eq!(h.player.take_start_position(), Some(10.0));
    assert_eq!(
        h.player.pending_resume_seek, None,
        "left armed, the next play seeks back to 45 and the queued seek is lost twice over"
    );
    assert!(h.player.user_seek_override.is_none());
}

#[cfg(target_os = "windows")]
#[tokio::test]
async fn the_auto_resume_is_taken_when_no_seek_was_queued() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());
    h.player.pending_resume_seek = Some(45.0);

    assert_eq!(h.player.take_start_position(), Some(45.0));
    assert_eq!(h.player.pending_resume_seek, None);
}

/// The whole chain a queued seek has to survive, which nothing exercised in one piece: a
/// parked switch, the shared fallback, a refusal, the reap, and the backend that spends it.
#[cfg(target_os = "windows")]
#[tokio::test]
async fn a_queued_seek_survives_a_parked_switch_through_the_reap() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());
    h.player.is_playing = false;
    // No buffer to reuse: the switch takes its still-streaming arm and spawns no decoder.
    h.player.current_buffer = None;
    h.player.is_asio_mode = true;
    h.player.asio_handle = None;
    h.player.pending_device_switch = Some(("dev-1".to_string(), crate::player::OutputMode::Asio));
    h.player.user_seek_override = Some(("track-1".to_string(), 45.0));
    // A live track the switch can re-arm. `has_track` without one is a DASH-only state, and
    // every device switch refuses it.
    let _track = ReplayableTrack::set("https://example.invalid/track-1.flac");

    let started = h.player.start_asio_playback(
        crate::player::buffer::RamBuffer::from_complete(vec![1, 2, 3]),
        true,
    );
    assert!(!started, "a parked switch owns the next handle");
    assert!(!h.player.is_asio_mode, "so this load plays shared");

    // Standing in for the shared fallback, which needs a real device to reach.
    if let Some(queued) = h.player.take_user_seek_override() {
        h.player.pending_resume_seek = Some(queued);
    }
    h.player.pre_seek_pos = h.player.pending_resume_seek;

    // The reader refuses it: the buffer has not reached 45s yet.
    h.decode_events
        .send(DecodeEvent::SeekComplete {
            gen_id: h.player.seek_ack_gen,
            position: 0.0,
            refused: true,
        })
        .unwrap();
    h.player.poll_playback();
    assert_eq!(
        h.player.pending_resume_seek,
        Some(45.0),
        "a refusal retires the marker, never the intent: another backend can still serve it"
    );

    h.player.asio_teardown = Some(std::thread::spawn(|| {}));
    while !h.player.asio_teardown.as_ref().unwrap().is_finished() {
        std::thread::yield_now();
    }
    h.player.poll_asio_teardown();
    assert!(
        h.player.pending_device_switch.is_none(),
        "switch dispatched"
    );
    assert!(h.player.is_asio_mode, "ASIO owns the session again");

    assert_eq!(
        h.player.take_start_position(),
        Some(45.0),
        "the seek the listener asked for outlived the fallback, the refusal and the reap"
    );
}

/// A load announces where it will open before the backend that consumes the pair is chosen.
/// Reading only the auto-resume there told the bar 45s while the stream opened at 10s.
#[cfg(target_os = "windows")]
#[tokio::test]
async fn the_announced_start_position_is_the_one_the_stream_opens_at() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());
    h.player.user_seek_override = Some(("track-1".to_string(), 10.0));
    h.player.pending_resume_seek = Some(45.0);

    assert_eq!(h.player.start_position(), Some(10.0));
    assert_eq!(
        h.player.take_start_position(),
        Some(10.0),
        "announcing one position and opening at another is the defect itself"
    );
}

/// A refused seek moved nothing, and decoding runs a ring ahead of the speakers. Persisting
/// what the decoder reported would store that lead as if it had been heard, and rebasing the
/// played counter on it would teleport the position by the same amount.
#[tokio::test]
async fn a_refused_seek_persists_what_was_played_not_what_was_decoded() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());
    h.player.is_playing = false;
    h.player.sample_rate = 1;
    h.player.channels = 1;
    h.player.played_samples.store(30, Relaxed);
    h.player.decoded_samples.store(32, Relaxed);

    h.player.handle_seek(120.0);
    h.decode_events
        .send(DecodeEvent::SeekComplete {
            gen_id: h.player.seek_ack_gen,
            position: 32.0,
            refused: true,
        })
        .unwrap();
    h.player.poll_playback();

    assert_eq!(h.player.resume_store.get("track-1"), Some(30.0));
    assert_eq!(
        h.player.played_samples.load(Relaxed),
        30,
        "a refusal flushed no ring, so there is nothing to rebase onto"
    );
}

/// The tick that reports a landing returns early on a paused stream, so the settle has to carry
/// the position itself. Without it the bar keeps the target `handle_seek` announced, for as long
/// as the pause lasts. Both bypass backends already answer this way.
#[tokio::test]
async fn a_seek_answered_while_paused_reports_the_landing() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());
    h.player.is_playing = false;
    h.player.sample_rate = 1;
    h.player.channels = 1;
    h.player.played_samples.store(30, Relaxed);
    h.player.decoded_samples.store(32, Relaxed);

    h.player.handle_seek(120.0);
    h.decode_events
        .send(DecodeEvent::SeekComplete {
            gen_id: h.player.seek_ack_gen,
            position: 32.0,
            refused: true,
        })
        .unwrap();
    h.player.poll_playback();

    let positions: Vec<f64> = h
        .events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|ev| match ev {
            PlayerEvent::TimeUpdate(t, _) => Some(*t),
            _ => None,
        })
        .collect();
    assert_eq!(
        positions,
        vec![120.0, 30.0],
        "the announced target is owed the position the seek actually settled on"
    );
}

/// `stop_decode` joins the decode thread on the player thread. A decoder parked in a starved
/// read answers no command; the stop has to carry the retire signal, or the join waits with
/// it. Asserting the flag before the join keeps a regression a failure rather than a hang.
#[tokio::test]
async fn stopping_the_decoder_retires_its_reader() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());
    // Held for the test: dropping the writer cancels the buffer, which would free the reader
    // for a different reason than the one under test.
    let (buffer, _writer) = crate::player::buffer::RamBuffer::new_for_test(1024);
    let cancel = Arc::new(AtomicBool::new(false));
    let mut reader = buffer.clone().with_reader_cancel(cancel.clone());
    h.player.current_buffer = Some(buffer);
    h.player.decode_reader_cancel = Some(cancel.clone());

    let reading = std::thread::spawn(move || {
        let mut sink = [0u8; 64];
        std::io::Read::read(&mut reader, &mut sink)
    });
    // Let it reach the wait: signalling before the read starts proves nothing about waking a
    // parked reader.
    std::thread::sleep(std::time::Duration::from_millis(50));

    h.player.stop_decode();

    assert!(
        cancel.load(Relaxed),
        "a stop that does not retire the reader leaves the join waiting on the network"
    );
    let outcome = reading.join().expect("the reader thread must not panic");
    assert_eq!(
        outcome
            .expect_err("a retired reader reports instead of returning bytes")
            .kind(),
        std::io::ErrorKind::Other,
        "never Interrupted: symphonia's read_buf_exact retries that kind forever"
    );
}

/// A failed load clears the flag every position emitter keys on, so the fields describing that
/// position have to fall with it. The seek answer is the shortest path to the consequence: it
/// would otherwise carry the replaced track's position under the failed load's seq.
#[tokio::test]
async fn a_failed_load_leaves_no_position_from_the_track_it_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());
    h.player.sample_rate = 1;
    h.player.channels = 1;
    // Where the previous track stood when this load tore its pipeline down.
    h.player.played_samples.store(30, Relaxed);
    h.player.current_duration = 240.0;
    h.player.decode_cmd_tx = None;

    h.player.abandon_failed_load();
    h.player.handle_seek(120.0);

    assert_eq!(
        h.player.current_duration, 0.0,
        "a length belongs to a track, and the one it was measured on is gone"
    );
    let positions: Vec<f64> = h
        .events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|ev| match ev {
            PlayerEvent::TimeUpdate(t, _) => Some(*t),
            _ => None,
        })
        .collect();
    assert_eq!(
        positions,
        vec![0.0],
        "the answer cannot be where a track this load already replaced stood"
    );
}

/// A pre-seek the reader refused moved nothing. Announcing the request at play time named a
/// position playback never reached, and only the next periodic tick took it back.
#[tokio::test]
async fn a_refused_pre_seek_is_not_announced_as_the_start_position() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());
    h.player.is_playing = false;
    // What a load leaves behind once it has dispatched its pre-seek.
    h.player.pending_resume_seek = Some(45.0);
    h.player.pre_seek_pos = Some(45.0);

    h.decode_events
        .send(DecodeEvent::SeekComplete {
            gen_id: h.player.seek_ack_gen,
            position: 0.0,
            refused: true,
        })
        .unwrap();
    h.player.poll_playback();
    assert_eq!(
        h.player.pre_seek_pos, None,
        "the refusal retires the marker"
    );

    h.player.handle_play();

    assert!(
        !h.events
            .lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e, PlayerEvent::TimeUpdate(t, _) if (*t - 45.0).abs() < 0.01)),
        "45s was asked for and refused; the decoder never went there"
    );
}

/// Two seeks in flight: `SeekComplete` used to carry no identity, so the stale first answer
/// could settle the pin and persist its own position over the seek still being awaited.
#[tokio::test]
async fn a_superseded_seek_answer_settles_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());
    // Paused, where the periodic tick cannot quietly correct the stored position.
    h.player.is_playing = false;

    h.player.handle_seek(30.0);
    let superseded = h.player.seek_ack_gen;
    h.player.handle_seek(120.0);

    h.decode_events
        .send(DecodeEvent::SeekComplete {
            gen_id: superseded,
            position: 30.0,
            refused: false,
        })
        .unwrap();
    h.player.poll_playback();

    assert!(
        h.player.seeking,
        "the pin belongs to the seek still in flight, not to the one that just answered"
    );
    assert_eq!(h.player.seek_target, Some(120.0));
    assert_eq!(
        h.player.resume_store.get("track-1"),
        None,
        "a superseded answer's position must never become the resume point"
    );

    h.decode_events
        .send(DecodeEvent::SeekComplete {
            gen_id: h.player.seek_ack_gen,
            position: 120.0,
            refused: false,
        })
        .unwrap();
    h.player.poll_playback();

    assert!(!h.player.seeking);
    assert_eq!(
        h.player.resume_store.get("track-1"),
        Some(120.0),
        "the awaited answer is the one that settles and persists"
    );
}

#[tokio::test]
async fn a_seek_settled_after_a_stop_keeps_the_stopped_state() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());

    h.player.handle_stop(0);
    h.player.handle_seek(30.0);
    h.decode_events
        .send(DecodeEvent::SeekComplete {
            gen_id: h.player.seek_ack_gen,
            position: 30.0,
            refused: false,
        })
        .unwrap();
    h.player.poll_playback();

    assert_eq!(
        last_state(&h.events),
        Some(PlaybackState::Stopped),
        "a stop retains the pipeline, so it reads as a pause unless the state is carried"
    );
}

/// Exclusive seeks used to sit outside the seek protocol entirely: no pin, no announcement.
/// The render's pre-seek position walked the bar back, and the frontend had to guess.
#[cfg(target_os = "windows")]
#[tokio::test]
async fn an_exclusive_seek_pins_and_announces_only_once_dispatched() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());
    h.player.is_exclusive_mode = true;

    // No decoder to receive it: announcing would pin a target that can never converge.
    h.player.handle_seek(30.0);
    assert!(
        !h.player.seeking,
        "a seek nobody received must not pin the UI"
    );
    assert_eq!(h.player.seek_target, None);

    let (tx, rx) = mpsc::channel();
    h.player.exclusive_seek_tx = Some(tx);
    h.player.handle_seek(45.0);

    assert_eq!(rx.try_recv().ok().map(|(target, _gen)| target), Some(45.0));
    assert!(h.player.seeking);
    assert_eq!(h.player.seek_target, Some(45.0));
    assert_eq!(
        last_state(&h.events),
        Some(PlaybackState::Seeking),
        "a dispatched seek is announced; its settle then has something to close"
    );
}

/// The bypass paths returned from `handle_load` without announcing anything, where the shared
/// path announces `Ready`. The frontend kept its last `active` and ran its own clock through a
/// device open that costs seconds on a rate-locked interface, then the bar snapped back.
#[cfg(target_os = "windows")]
#[tokio::test]
async fn a_bypass_load_announces_that_nothing_plays_yet() {
    // Same reason as its siblings: the generation sampled below has to still be current when
    // `handle_load` re-reads it, or the branch under test is never reached at all.
    let _serialised = crate::audio::preload::tests::PRELOAD_TESTS.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());
    h.player.is_exclusive_mode = true;
    // No handle, so the branch is taken and spawns nothing. That is the window under test: the
    // device is not open, and nothing downstream can speak for it until it is.
    h.player.exclusive_handle = None;

    let delivered = h.player.handle_load(
        LoadRequest {
            buffer: crate::player::buffer::RamBuffer::from_complete(vec![1, 2, 3]),
            load_gen: crate::player::current_gen(),
            seq: 1,
            track_id: "track-1".to_string(),
            product_id: None,
            resume_policy: ResumePolicy::Restart,
            load_start: std::time::Instant::now(),
            cached: true,
            format: "flac".to_string(),
        },
        false,
    );

    assert!(delivered, "the exclusive branch owns this load");
    assert_eq!(
        last_state(&h.events),
        Some(PlaybackState::Ready),
        "a track whose device has not opened yet must not read as playing"
    );
}

/// ASIO dispatched its seeks without announcing them while exclusive and the shared path both
/// did. The same drag showed a stalled transport on two backends and nothing on the third.
#[cfg(target_os = "windows")]
#[tokio::test]
async fn an_asio_seek_pins_and_announces_only_once_dispatched() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());
    h.player.is_asio_mode = true;

    // No decoder to receive it: announcing would pin a target that can never converge.
    h.player.handle_seek(30.0);
    assert!(
        !h.player.seeking,
        "a seek nobody received must not pin the UI"
    );
    assert_eq!(h.player.seek_target, None);
    assert_eq!(
        last_state(&h.events),
        None,
        "nothing dispatched means no settle is owed"
    );

    let (tx, rx) = mpsc::channel();
    h.player.asio_seek_tx = Some(tx);
    h.player.handle_seek(45.0);

    assert_eq!(rx.try_recv().ok().map(|(target, _gen)| target), Some(45.0));
    assert!(h.player.seeking);
    assert_eq!(h.player.seek_target, Some(45.0));
    assert_eq!(
        last_state(&h.events),
        Some(PlaybackState::Seeking),
        "a dispatched seek is announced; its settle then has something to close"
    );
}

/// A park clears the seek channel with the handle. A Play landing in that window used to
/// hand the UI the queued position as though a decoder had taken it.
#[cfg(target_os = "windows")]
#[tokio::test]
async fn a_play_during_an_asio_park_reports_the_real_position() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());
    h.player.is_asio_mode = true;

    // The park took the handle and cleared the channel behind it. With `current_buffer` None,
    // no respawn can carry the position either: nothing at all can honour this seek.
    h.player.asio_seek_tx = None;
    h.player.last_asio_pos = Some(37.5);
    h.player.pending_resume_seek = Some(120.0);

    h.player.handle_play();

    let positions: Vec<f64> = h
        .events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|ev| match ev {
            PlayerEvent::TimeUpdate(t, _) => Some(*t),
            _ => None,
        })
        .collect();
    assert_eq!(
        positions,
        vec![37.5],
        "announcing 120.0 would report a position no decoder holds and none will reach"
    );
    assert_eq!(
        h.player.last_asio_pos,
        Some(37.5),
        "the fallback reports where playback stands without adopting the refused target"
    );
}

#[tokio::test]
async fn a_seek_queued_behind_a_load_persists_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());

    // No pipeline and no retained track: the seek can only wait for a load, and that load
    // may never land.
    h.player.decode_cmd_tx = None;
    h.player.has_track = false;

    h.player.handle_seek(120.0);

    assert_eq!(
        h.player.pending_resume_seek,
        Some(120.0),
        "with a load still to come the seek is queued rather than refused"
    );
    assert_eq!(
        h.player.resume_store.get("track-1"),
        None,
        "a queued seek is not an accepted one: persisting it would resume at a position \
         playback never reached if the load never arrives"
    );
}

/// The shape natural completion leaves behind. The IPC handler flushes the seek target to the
/// frontend before the thread ever sees it, so a refusal that says nothing leaves the bar on a
/// position playback never reached, for as long as no load lands. The counter is set apart from
/// the target so the answer cannot be a constant.
#[tokio::test]
async fn a_seek_with_nothing_loaded_answers_with_the_played_position() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());
    h.player.decode_cmd_tx = None;
    h.player.has_track = false;
    h.player.current_track_id = None;
    h.player.sample_rate = 1;
    h.player.channels = 1;
    h.player.played_samples.store(30, Relaxed);

    h.player.handle_seek(120.0);

    let positions: Vec<f64> = h
        .events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|ev| match ev {
            PlayerEvent::TimeUpdate(t, _) => Some(*t),
            _ => None,
        })
        .collect();
    assert_eq!(
        positions,
        vec![30.0],
        "a seek nothing can serve is still owed the position playback stands at"
    );
}

/// The other way into the same branch: a fatal decode error nulled the sender while a newer load
/// is still fetching. That load's own `handle_load` overwrites the queued target before any
/// decoder reads it, so the queue cannot be what answers the frontend here either.
#[tokio::test]
async fn a_seek_queued_behind_a_load_answers_the_optimistic_jump() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());
    h.player.decode_cmd_tx = None;
    h.player.loading_gen = Some(7);
    h.player.sample_rate = 1;
    h.player.channels = 1;
    h.player.played_samples.store(30, Relaxed);

    h.player.handle_seek(120.0);

    assert_eq!(
        h.player.pending_resume_seek,
        Some(120.0),
        "the intention outlives the refusal: a respawn that skips a load reads this slot"
    );
    let positions: Vec<f64> = h
        .events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|ev| match ev {
            PlayerEvent::TimeUpdate(t, _) => Some(*t),
            _ => None,
        })
        .collect();
    assert_eq!(
        positions,
        vec![30.0],
        "answering with the queued target would confirm a jump the incoming load discards"
    );
}

#[tokio::test]
async fn a_seek_onto_a_dead_pipeline_puts_the_real_position_back() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());

    // A fatal decode error nulls the sender while the track keeps draining, and no load
    // is coming to apply a queued position.
    h.player.decode_cmd_tx = None;

    h.player.handle_seek(120.0);

    assert!(
        !h.player.seeking,
        "no decoder can answer: nothing may claim a seek is in flight"
    );
    assert_eq!(
        h.player.pending_resume_seek, None,
        "queuing here is a lie: the next load overwrites the queue"
    );
    assert_eq!(
        h.player.resume_store.get("track-1"),
        None,
        "handle_seek persists the target before it knows the seek can happen; a refused \
         seek must not survive into the next cold start"
    );
    let positions: Vec<f64> = h
        .events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|ev| match ev {
            PlayerEvent::TimeUpdate(t, _) => Some(*t),
            _ => None,
        })
        .collect();
    assert_eq!(
        positions,
        vec![0.0],
        "the UI's optimistic jump has to be corrected, not left standing"
    );
}

#[tokio::test]
async fn a_seek_settled_before_the_first_play_announces_ready() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());

    // A fresh load announced Ready and was never played; the SDK seeks before the first
    // play (boombox applies assetPosition after the load).
    h.player.is_playing = false;

    h.player.handle_seek(30.0);
    h.decode_events
        .send(DecodeEvent::SeekComplete {
            gen_id: h.player.seek_ack_gen,
            position: 30.0,
            refused: false,
        })
        .unwrap();
    h.player.poll_playback();

    assert_eq!(
        last_state(&h.events),
        Some(PlaybackState::Ready),
        "a track that never started is Ready; Paused would claim it had been playing"
    );
}

#[tokio::test]
async fn a_seek_settled_after_a_pause_before_the_first_play_keeps_paused() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());

    // Paused without ever having played, and that pause was announced. The seek that
    // interrupts it must put Paused back; restoring the load's Ready would erase it.
    h.player.is_playing = false;
    h.player.handle_pause();

    h.player.handle_seek(30.0);
    h.decode_events
        .send(DecodeEvent::SeekComplete {
            gen_id: h.player.seek_ack_gen,
            position: 30.0,
            refused: false,
        })
        .unwrap();
    h.player.poll_playback();

    assert_eq!(
        last_state(&h.events),
        Some(PlaybackState::Paused),
        "a settled seek restores what it interrupted, and that was an explicit pause"
    );
}

#[tokio::test]
async fn a_decode_error_while_paused_reaches_a_terminal_state() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());

    // The shape that defers its terminal state to the drain path: EOF already reported and
    // audio already played. Paused, that path never runs.
    h.player.pending_complete = true;
    h.player.played_samples.store(1_000, Relaxed);
    h.player.handle_pause();

    h.decode_events
        .send(DecodeEvent::Error("decode fatal".to_string()))
        .unwrap();
    h.player.poll_playback();

    assert!(!h.player.has_track, "the settle tears the pipeline down");
    assert_eq!(
        last_state(&h.events),
        Some(PlaybackState::Stopped),
        "a media error with no terminal state behind it leaves the transport reading paused"
    );
}

#[tokio::test]
async fn a_seek_in_flight_holds_off_the_drain() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());

    // Decode reported EOF, and the seek's mute has frozen the played count since.
    h.player.pending_complete = true;
    h.player.played_samples.store(100, Relaxed);
    h.player.last_played_snapshot = 100;
    h.player.handle_seek(30.0);
    h.player.poll_playback();

    assert!(
        h.player.has_track,
        "a played count frozen by the mute is not a drained ring"
    );
}

#[tokio::test]
async fn retiring_the_decoder_clears_a_pending_seek() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());

    h.player.handle_seek(30.0);
    assert!(h.player.seeking);

    h.player.stop_decode();

    assert!(!h.player.seeking, "no decoder is left to answer that seek");
    assert_eq!(h.player.seek_target, None);
}

#[tokio::test]
async fn a_fatal_error_during_a_seek_settles_instead_of_stranding_it() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());

    // The shape that ordinarily defers its terminal state to the drain path.
    h.player.pending_complete = true;
    h.player.played_samples.store(1_000, Relaxed);
    h.player.handle_seek(30.0);

    h.decode_events
        .send(DecodeEvent::Error("decode fatal".to_string()))
        .unwrap();
    h.player.poll_playback();

    assert!(!h.player.seeking, "a dead decoder never answers the seek");
    assert!(!h.player.has_track);
}

#[tokio::test]
async fn a_seek_after_a_deferred_decode_error_still_lets_the_track_finish() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());

    // Audio already played: the settle hands the terminal state to the drain rather than
    // firing now. The decode thread has exited regardless.
    h.player.pending_complete = true;
    h.player.played_samples.store(1_000, Relaxed);
    h.decode_events
        .send(DecodeEvent::Error("decode fatal".to_string()))
        .unwrap();
    h.player.poll_playback();

    assert!(h.player.has_track, "the terminal state waits on the drain");
    assert!(
        h.player.decode_cmd_tx.is_none(),
        "the thread that reported the error is already gone"
    );

    // A seek landing in that window must not arm the guard the drain reads.
    h.player.handle_seek(30.0);
    assert!(!h.player.seeking, "nothing is left to answer that seek");

    h.player.poll_playback();
    assert!(
        !h.player.has_track,
        "the drain completes rather than stranding the track"
    );
}

#[tokio::test]
async fn a_completed_track_leaves_no_decoder_behind() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());

    // The ring has drained: decode reported EOF and the played count stopped moving.
    h.player.pending_complete = true;
    h.player.played_samples.store(100, Relaxed);
    h.player.last_played_snapshot = 100;
    h.player.poll_playback();

    assert!(!h.player.has_track, "the drain commits the completion");
    assert!(
        h.player.decode_cmd_tx.is_none(),
        "a decoder parked at EOF outlives the track unless completion retires it"
    );
}

#[tokio::test]
async fn a_seek_reviving_a_parked_decoder_disarms_the_pending_completion() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());

    // What the decode thread parking at EOF leaves behind: the completion is armed, and
    // the seek that follows brings the stream back to life.
    h.player.pending_complete = true;
    h.player.handle_seek(30.0);
    h.decode_events
        .send(DecodeEvent::SeekComplete {
            gen_id: h.player.seek_ack_gen,
            position: 30.0,
            refused: false,
        })
        .unwrap();
    h.player.poll_playback();

    assert!(
        !h.player.pending_complete,
        "a revived stream read as draining reports Completed on the first frozen played count"
    );
}

#[tokio::test]
async fn decode_reaching_eof_while_paused_keeps_the_resume_position() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());

    h.player.resume_store.set("track-1", 42.0);
    h.player.handle_pause();

    h.decode_events.send(DecodeEvent::Finished).unwrap();
    h.player.poll_playback();

    assert_eq!(
        h.player.resume_store.get("track-1"),
        Some(42.0),
        "the decoder reaches EOF a ring buffer ahead of the audio, so its EOF is not the \
         listener's"
    );
}

/// An ASIO player holding one live stream, with the driver's event channel in the test's hand.
/// The cancel token is the live decoder's: whether a refusal reaches it is the whole question.
#[cfg(target_os = "windows")]
fn asio_harness(
    dir: &std::path::Path,
    live_stream: u32,
) -> (Harness, mpsc::Sender<AsioEvent>, Arc<AtomicBool>) {
    let mut h = harness(dir);
    // The command receiver is dropped: every send through the handle is already best-effort.
    let (handle, asio_events, _) = AsioHandle::for_test();
    let cancel = Arc::new(AtomicBool::new(false));
    h.player.is_asio_mode = true;
    h.player.asio_handle = Some(handle);
    h.player.current_asio_stream_id = Some(live_stream);
    h.player.asio_stream_cancel = Some(cancel.clone());
    (h, asio_events, cancel)
}

/// A rate refusal from a stream already superseded must not reach the track that replaced it.
/// The exclusive twin re-arms shared on any refusal, and may: its own takes the one render
/// thread down whichever stream it judged, so the re-arm is owed either way. No ASIO refusal
/// does that, `finish_rebuild` and the reset give-up both leaving the control thread alive.
#[cfg(target_os = "windows")]
#[tokio::test]
async fn a_superseded_asio_rate_refusal_leaves_the_live_track_alone() {
    let dir = tempfile::tempdir().unwrap();
    let (mut h, asio_events, cancel) = asio_harness(dir.path(), 2);

    asio_events
        .send(AsioEvent::RateUnsupported { stream_id: Some(1) })
        .unwrap();
    h.player.poll_asio_events();

    assert!(
        h.player.is_asio_mode,
        "a stale refusal turned ASIO off for the track that replaced the refused one"
    );
    assert!(
        !cancel.load(Relaxed),
        "a stale refusal cancelled the decoder of a track nothing refused"
    );
    assert_eq!(
        h.player.asio_skip_track, None,
        "a stale refusal condemned the live track"
    );
}

/// A format refusal is scoped for the same reason: its channel-count half reads the TRACK's
/// channel count, and a superseded stream's verdict must not condemn the track that replaced it.
#[cfg(target_os = "windows")]
#[tokio::test]
async fn a_superseded_asio_format_refusal_leaves_the_live_track_alone() {
    let dir = tempfile::tempdir().unwrap();
    let (mut h, asio_events, cancel) = asio_harness(dir.path(), 2);

    asio_events
        .send(AsioEvent::FormatUnsupported { stream_id: Some(1) })
        .unwrap();
    h.player.poll_asio_events();

    assert!(
        h.player.is_asio_mode,
        "a stale refusal turned ASIO off for the track that replaced the refused one"
    );
    assert!(
        !cancel.load(Relaxed),
        "a stale refusal cancelled the decoder of a track nothing refused"
    );
}

/// The same refusal, named for the stream still live, keeps every effect it ever had.
#[cfg(target_os = "windows")]
#[tokio::test]
async fn the_live_streams_asio_format_refusal_still_falls_back_to_shared() {
    let dir = tempfile::tempdir().unwrap();
    let (mut h, asio_events, cancel) = asio_harness(dir.path(), 2);

    asio_events
        .send(AsioEvent::FormatUnsupported { stream_id: Some(2) })
        .unwrap();
    h.player.poll_asio_events();

    assert!(!h.player.is_asio_mode, "the refused track stayed on ASIO");
    assert!(cancel.load(Relaxed), "the refused stream's decoder ran on");
}

/// The same for a rate refusal, which also condemns the track against re-engaging ASIO.
#[cfg(target_os = "windows")]
#[tokio::test]
async fn the_live_streams_asio_rate_refusal_still_falls_back_to_shared() {
    let dir = tempfile::tempdir().unwrap();
    let (mut h, asio_events, cancel) = asio_harness(dir.path(), 2);

    asio_events
        .send(AsioEvent::RateUnsupported { stream_id: Some(2) })
        .unwrap();
    h.player.poll_asio_events();

    assert!(!h.player.is_asio_mode, "the refused track stayed on ASIO");
    assert!(cancel.load(Relaxed), "the refused stream's decoder ran on");
    assert_eq!(
        h.player.asio_skip_track.as_deref(),
        Some("track-1"),
        "the refused track was left free to re-engage ASIO and refuse again"
    );
}

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
#[tokio::test]
async fn a_promotion_publishes_the_generation_its_fade_was_armed_under() {
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

    h.player.promote_crossfade(44_100 * 2 * 6);

    let retained = crate::state::CURRENT_TRACK
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
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
#[tokio::test]
async fn a_promotion_publishes_the_three_events_the_sdk_waits_on() {
    let dir = tempfile::tempdir().unwrap();
    let _slot = CurrentTrackSlot::clear();
    let mut h = harness(dir.path());
    let _incoming_cmds = arm_for_promotion(&mut h, 246.0);

    h.player.promote_crossfade(44_100 * 2 * 6);

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
#[tokio::test]
async fn a_promotion_emits_the_incoming_position_before_the_transition_events() {
    let dir = tempfile::tempdir().unwrap();
    let _slot = CurrentTrackSlot::clear();
    let mut h = harness(dir.path());
    let _incoming_cmds = arm_for_promotion(&mut h, 246.0);

    // Six seconds of the incoming track were drained during the overlap, and the
    // swapping tick left that on the clock. Priming it is what the position report
    // reads: the promotion no longer writes the clock, and without this the fixture's
    // outgoing total is what would be reported.
    h.player.played_samples.store(44_100 * 2 * 6, Relaxed);
    h.player.promote_crossfade(44_100 * 2 * 6);

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
    h.player.promote_crossfade(44_100 * 2 * 6);

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
/// track. Had the poll run first the fade would have promoted; both orderings must agree.
#[tokio::test]
async fn a_cancellation_promotes_a_fade_the_callback_already_finished() {
    let dir = tempfile::tempdir().unwrap();
    let _slot = CurrentTrackSlot::clear();
    let mut h = harness(dir.path());
    let _incoming_cmds = arm_for_promotion(&mut h, 246.0);
    // What the callback leaves behind when it completes a fade on its own clock.
    h.player.cpal_xfade = Some(crate::player::thread::output::CrossfadeLink {
        attach: Arc::new(Mutex::new(None)),
        cancel: Arc::new(AtomicBool::new(false)),
        out_eof: Arc::new(AtomicBool::new(false)),
        in_eof: Arc::new(AtomicBool::new(false)),
        done: Arc::new(std::sync::atomic::AtomicU64::new(
            crate::player::thread::output::pack_xfade_done(1, 44_100 * 2 * 6),
        )),
    });

    h.player.cancel_crossfade();

    assert!(
        h.player.pending_crossfade.is_none(),
        "the fade is spent either way; what differs is which track survives it"
    );
    let events = h.events.lock().unwrap();
    assert!(
        events
            .iter()
            .any(|ev| matches!(ev, PlayerEvent::CrossfadePromoted(..))),
        "a finished fade must promote, not cancel, got {events:?}"
    );
}

/// A Load the generation gate discards must leave a live fade alone. Tearing it down first
/// retracts the attach slot and stops the incoming decoder, and nothing legitimate follows to
/// justify it: a generation can be minted and then die to an HTTP error (`ctx.fail_load`), and
/// neither `LoadFailed` nor `EmitMaxConnections` cancels a fade. The discarded Load is then the
/// sole cause of the loss, and the outgoing gain snaps back mid-ramp.
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
            cached: true,
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

/// The refusal memo names the track a fade was already refused for, and `arm_crossfade` is its
/// only reader: it exists to keep the refused decision from being re-run four times a second
/// for the rest of a track's tail. A Load that changes nothing supersedes no refusal: clearing
/// it there buys back the work the memo was written to avoid.
#[tokio::test]
async fn a_stale_load_keeps_the_refusal_it_did_not_supersede() {
    let _serialised = crate::audio::preload::tests::PRELOAD_TESTS.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());
    let refused = "https://example.invalid/refused";
    h.player.xfade_refused_url = Some(refused.to_string());

    let superseded = crate::player::current_gen().wrapping_sub(1);

    h.player.handle_load(
        LoadRequest {
            buffer: crate::player::buffer::RamBuffer::from_complete(vec![1, 2, 3]),
            load_gen: superseded,
            seq: 12,
            track_id: "track-superseded".to_string(),
            product_id: None,
            resume_policy: ResumePolicy::Restart,
            load_start: std::time::Instant::now(),
            cached: true,
            format: "flac".to_string(),
        },
        false,
    );

    assert_eq!(
        h.player.xfade_refused_url.as_deref(),
        Some(refused),
        "a Load the gate discards forgot a refusal it never superseded"
    );
}

/// Arming polls four times a second for the whole tail of every track, and each tick asks the
/// cache whether the next track is on disk. `MenuCommand::ClearCache` holds that same mutex for
/// its entire `remove_dir_all`, deliberately, to keep the wipe atomic against
/// `store_finished_ciphertext`'s unlocked writes: waiting for it here parks the player control
/// thread for the whole wipe.
#[tokio::test]
async fn a_busy_cache_never_parks_the_arming_tick() {
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
            crate::player::thread::output::pack_xfade_done(1, 44_100 * 2 * 6),
        )),
    }
}

/// The listener changing their mind and the incoming track dying are two different
/// cancellations, and a fade the callback has already finished is where they part. The promotion
/// is forced either way, but here the track promoted is the one whose decode thread has just
/// returned. Nothing else can ever report it: `Finished` never comes from a thread that exited,
/// and the drain the completion path waits on never arrives. Left unsettled, the UI names that
/// track Active over permanent silence, with no error and no skip.
#[tokio::test]
async fn a_promoted_fade_settles_the_incoming_track_that_died() {
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
                .any(|ev| matches!(ev, PlayerEvent::NetworkLost)),
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
            .any(|ev| matches!(ev, PlayerEvent::NetworkLost)),
        "a skip is not a failure: promoting for one must report nothing, got {events:?}"
    );
}

/// The outgoing counter stopped 2.0s short of the end while a fade was running, the drain check
/// read that as a finished track, and the teardown dropped the cpal stream BOTH tracks played
/// through. The counter cannot describe a fade (`fade_pos` advances on the callback's clock,
/// `played_samples` only on what the outgoing ring supplied): a fade is left to its own promotion.
#[tokio::test]
async fn a_stalled_counter_does_not_complete_a_track_that_is_still_fading() {
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

/// The overlap is bounded by what remained of the OUTGOING track and never by the
/// incoming track's own length, and `state.duration` is only a probe estimate: a
/// container that undercounts its frames drains more than it declared. The periodic
/// emitter clamps for this reason; the promotion's own report has to as well, or
/// the bar jumps past the end of a track that just started.
#[tokio::test]
async fn a_promotion_never_reports_a_position_past_the_incoming_duration() {
    let dir = tempfile::tempdir().unwrap();
    let _slot = CurrentTrackSlot::clear();
    let mut h = harness(dir.path());
    // A five-second incoming track, six seconds of it drained during the overlap.
    let _incoming_cmds = arm_for_promotion(&mut h, 5.0);
    // The clock the swapping tick left behind. Without it the fixture's outgoing
    // total is what gets clamped, and the assertion would pass on saturation instead
    // of on the overlap it means to describe.
    h.player.played_samples.store(44_100 * 2 * 6, Relaxed);

    h.player.promote_crossfade(44_100 * 2 * 6);

    let events = h.events.lock().unwrap();
    let reported = events
        .iter()
        .find_map(|ev| match ev {
            PlayerEvent::TimeUpdate(secs, _) => Some(*secs),
            _ => None,
        })
        .expect("the promotion must report the incoming track's position");
    assert!(
        (reported - 5.0).abs() < 1e-6,
        "the report must be clamped to the incoming duration, got {reported}"
    );
}

/// A swap only happens once the outgoing decoder has reported `Finished`, which also sets
/// `pending_complete`. Carried onto the incoming track it suppresses every resume-store write for
/// it and turns two stalled poll ticks into a false completion. The clock is the other half and
/// NOT this thread's to move: the callback rebased it in the tick that swapped and has credited
/// the promoted ring since, so a store here can only throw that interval away. Both halves are
/// checked against the state a real fade leaves behind.
#[tokio::test]
async fn a_promotion_keeps_the_clock_the_swap_set_and_clears_the_completion_state() {
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
    let in_played = 44_100 * 2 * 6;
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
#[tokio::test]
async fn a_promotion_retires_the_mirrors_that_described_the_outgoing_buffer() {
    let dir = tempfile::tempdir().unwrap();
    let _slot = CurrentTrackSlot::clear();
    let mut h = harness(dir.path());
    let _incoming_cmds = arm_for_promotion(&mut h, 246.0);
    h.player.buffer_stalled = true;
    h.player.xfade_refused_url = Some("https://example.invalid/refused".to_string());

    h.player.promote_crossfade(44_100 * 2 * 6);

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
#[tokio::test]
async fn a_promotion_reports_both_the_file_rate_and_the_output_rate() {
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

    h.player.promote_crossfade(44_100 * 2 * 6);

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
