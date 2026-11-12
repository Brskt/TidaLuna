//! Tests for `src/player/thread/transport.rs`, attached to it by `#[path]`. The player of the
//! shared `harness` fixture also drives `poll_playback`, which lives in the sibling `playback`
//! module.

use super::{PlayAction, decide_play};
use crate::player::thread::DecodeEvent;
use crate::player::thread::harness::{harness, last_state};
use crate::player::{PlaybackState, PlayerCommand, PlayerEvent};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::Relaxed;

/// A load in flight wins even when a retained source also exists.
#[test]
fn a_play_follows_the_live_track_then_the_load_then_the_retained_source() {
    for (has_track, loading_gen, retained, expected) in [
        (true, None, false, PlayAction::Resume),
        (true, Some(7), true, PlayAction::Resume),
        (false, Some(7), true, PlayAction::DeferTo(7)),
        (false, Some(3), false, PlayAction::DeferTo(3)),
        (false, None, true, PlayAction::ReArm),
        (false, None, false, PlayAction::Ignore),
    ] {
        assert_eq!(
            decide_play(has_track, loading_gen, retained),
            expected,
            "decide_play({has_track}, {loading_gen:?}, {retained})"
        );
    }
}

/// A refused seek moved nothing, and decoding runs a ring ahead of the speakers. Persisting
/// what the decoder reported would store that lead as if it had been heard, and rebasing the
/// played counter on it would teleport the position by the same amount.
#[test]
fn a_refused_seek_persists_what_was_played_not_what_was_decoded() {
    let mut h = harness();
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

/// The tick that reports a landing returns early on a paused stream: the settle has to carry
/// the position itself. Without it the bar keeps the target `handle_seek` announced, for as long
/// as the pause lasts. Both bypass backends already answer this way.
#[test]
fn a_seek_answered_while_paused_reports_the_landing() {
    let mut h = harness();
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
#[test]
fn stopping_the_decoder_retires_its_reader() {
    let mut h = harness();
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

/// A pre-seek the reader refused moved nothing. Announcing the request at play time named a
/// position playback never reached, and only the next periodic tick took it back.
#[test]
fn a_refused_pre_seek_is_not_announced_as_the_start_position() {
    let mut h = harness();
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

/// Two seeks in flight: `SeekComplete` used to carry no identity, and the stale first answer
/// could settle the pin and persist its own position over the seek still being awaited.
#[test]
fn a_superseded_seek_answer_settles_nothing() {
    let mut h = harness();
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

#[test]
fn a_seek_settled_after_a_stop_keeps_the_stopped_state() {
    let mut h = harness();

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

#[test]
fn a_seek_queued_behind_a_load_persists_nothing() {
    let mut h = harness();

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

/// A fatal decode error nulled the sender while a newer load is still fetching. The IPC handler
/// flushed the seek target to the frontend before the thread ever saw it, and that load's own
/// `handle_load` overwrites the queued target before any decoder reads it: the queue cannot be
/// what answers. The counter is set apart from the target, keeping the answer from being a constant.
#[test]
fn a_seek_queued_behind_a_load_answers_the_played_position() {
    let mut h = harness();
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

#[test]
fn a_seek_onto_a_dead_pipeline_puts_the_real_position_back() {
    let mut h = harness();

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

#[test]
fn a_seek_settled_before_the_first_play_announces_ready() {
    let mut h = harness();

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

#[test]
fn a_seek_settled_after_a_pause_before_the_first_play_keeps_paused() {
    let mut h = harness();

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

#[test]
fn a_decode_error_while_paused_reaches_a_terminal_state() {
    let mut h = harness();

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

#[test]
fn a_seek_in_flight_holds_off_the_drain() {
    let mut h = harness();

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

#[test]
fn retiring_the_decoder_clears_a_pending_seek() {
    let mut h = harness();

    h.player.handle_seek(30.0);
    assert!(h.player.seeking);

    h.player.stop_decode();

    assert!(!h.player.seeking, "no decoder is left to answer that seek");
    assert_eq!(h.player.seek_target, None);
}

#[test]
fn a_fatal_error_during_a_seek_settles_instead_of_stranding_it() {
    let mut h = harness();

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

#[test]
fn a_seek_after_a_deferred_decode_error_still_lets_the_track_finish() {
    let mut h = harness();

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

#[test]
fn a_completed_track_leaves_no_decoder_behind() {
    let mut h = harness();

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

#[test]
fn a_seek_reviving_a_parked_decoder_disarms_the_pending_completion() {
    let mut h = harness();

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

#[test]
fn decode_reaching_eof_while_paused_keeps_the_resume_position() {
    let mut h = harness();

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

/// A page that ends stops its track, which stays retained and resumable. The page that follows
/// presses Play for the track ITS queue names, and a resume of the retained one would play a track
/// nothing on screen names until that page's own load replaced it.
#[test]
fn a_play_after_the_page_that_loaded_the_track_ended_resumes_nothing() {
    let mut h = harness();

    h.player.handle_command(PlayerCommand::Stop(1));
    h.player.handle_command(PlayerCommand::Orphan);
    h.events.lock().unwrap().clear();
    h.player.handle_play();

    assert!(!h.player.is_playing, "the retained track was resumed");
    assert_ne!(last_state(&h.events), Some(PlaybackState::Active));
}

/// The control of the test above: the same stop and play, with the page still there.
#[test]
fn a_play_after_a_stop_resumes_the_retained_track() {
    let mut h = harness();

    h.player.handle_command(PlayerCommand::Stop(1));
    h.player.handle_play();

    assert!(h.player.is_playing);
    assert_eq!(last_state(&h.events), Some(PlaybackState::Active));
}

/// The intent a stop records for a quality swap's re-assert is the ended page's: a page that
/// loads the same track on boot finds it stopped, as a reload leaves everything.
#[test]
fn the_page_that_follows_finds_the_retained_track_stopped_not_to_be_resumed() {
    let mut h = harness();

    h.player.handle_command(PlayerCommand::Stop(1));
    assert!(
        h.player.resume_on_reassert,
        "a stop of a playing track records the intent"
    );
    h.player.handle_command(PlayerCommand::Orphan);

    assert!(!h.player.resume_on_reassert);
}

/// A page that loads the retained track again, by the idempotent branch of `Player::load`, names
/// it: the re-assert lifts the orphaning and, the page being new, resumes nothing by itself. One
/// that names another track than the committed one names nothing of this one's.
#[test]
fn a_reassert_lifts_the_orphaning_only_for_the_committed_track() {
    for (track_id, orphaned) in [("track-1", false), ("another", true)] {
        let mut h = harness();
        h.player.handle_command(PlayerCommand::Stop(1));
        h.player.handle_command(PlayerCommand::Orphan);

        h.player.handle_command(PlayerCommand::ReassertResume {
            want_play: false,
            product_id: None,
            track_id: track_id.to_string(),
        });

        assert_eq!(h.player.orphaned, orphaned, "re-assert of {track_id:?}");
        assert!(!h.player.is_playing, "re-assert of {track_id:?} resumed");
    }
}
