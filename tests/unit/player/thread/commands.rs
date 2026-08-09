//! Tests for `src/player/thread/commands.rs`, attached to it by `#[path]`. The harness at
//! the bottom also drives `poll_playback`, which lives in the sibling `playback` module:
//! both are `pub(in player::thread)`, so either file reaches the whole surface.

use super::{PlayAction, decide_play, settle_load};
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
    // the load failed (no handle_load delivered), so the deferred play
    // tagged with that gen must not dangle
    assert_eq!(settle_load(Some(5), Some(5), 5), (None, None));
}

#[test]
fn settle_leaves_a_deferred_play_for_a_different_gen() {
    assert_eq!(settle_load(None, Some(9), 5), (None, Some(9)));
}

// The transport methods are plain `&mut self` functions, so a test drives them directly:
// no run loop, no audio device, no decode thread. The decode thread's reports arrive on a
// channel the test owns. `#[tokio::test]` throughout, since GOVERNOR's init calls tokio::spawn.

type Callback = Box<dyn Fn(PlayerEvent) + Send>;
type Spy = Arc<Mutex<Vec<PlayerEvent>>>;

struct Harness {
    player: PlayerThread<Callback>,
    events: Spy,
    /// Stands in for the decode thread's event sender.
    decode_events: mpsc::Sender<DecodeEvent>,
    /// Both are held for the test's lifetime so the receivers the player owns never report
    /// a hung-up channel.
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

    h.decode_events.send(DecodeEvent::SeekComplete).unwrap();
    h.player.poll_playback();

    assert!(
        !h.player.seeking,
        "a paused poll has to read the decoder's answer; waiting for Play strands it"
    );
    assert_eq!(h.player.seek_target, None);
}

#[tokio::test]
async fn a_seek_settled_after_a_stop_keeps_the_stopped_state() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());

    h.player.handle_stop(0);
    h.player.handle_seek(30.0);
    h.decode_events.send(DecodeEvent::SeekComplete).unwrap();
    h.player.poll_playback();

    assert_eq!(
        last_state(&h.events),
        Some(PlaybackState::Stopped),
        "a stop retains the pipeline, so it reads as a pause unless the state is carried"
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

    // Audio already played, so the settle hands the terminal state to the drain rather than
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
    h.decode_events.send(DecodeEvent::SeekComplete).unwrap();
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
