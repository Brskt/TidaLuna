//! Tests for `src/player/thread/commands.rs`, attached to it by `#[path]`. The harness at
//! the bottom also drives `poll_playback`, which lives in the sibling `playback` module:
//! both are `pub(in player::thread)`: either file reaches the whole surface.

use super::{PlayAction, decide_play, resolve_start_position, settle_load};
#[cfg(target_os = "windows")]
use super::{ResumePolicy, queued_seek_survives};
use crate::player::resume::ResumeStore;
use crate::player::thread::{DecodeCommand, DecodeEvent, PlayerThread};
use crate::player::{PlaybackState, PlayerCommand, PlayerEvent};
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

/// A re-armable track in the process-wide slot, for one test. A device switch refuses a live
/// track it cannot replay; dropping restores the slot even when the test panics.
#[cfg(target_os = "windows")]
struct ReplayableTrack;

#[cfg(target_os = "windows")]
impl ReplayableTrack {
    fn set(url: &str) -> Self {
        *crate::state::CURRENT_TRACK
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(crate::state::TrackInfo {
            url: url.to_string(),
            key: String::new(),
            format: "flac".to_string(),
        });
        Self
    }
}

#[cfg(target_os = "windows")]
impl Drop for ReplayableTrack {
    fn drop(&mut self) {
        *crate::state::CURRENT_TRACK
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
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
