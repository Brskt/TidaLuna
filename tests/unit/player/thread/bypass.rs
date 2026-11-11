//! Tests for `src/player/thread/bypass/` (the queued seek and the settles in `mod.rs`, the two
//! backends in `exclusive.rs` and `asio.rs`), attached to its `mod.rs` by `#[path]`. The player
//! of the shared `harness` fixture also drives `poll_playback`, which lives in the sibling
//! `playback` module.

#[cfg(target_os = "windows")]
use super::queued_seek_survives;
#[cfg(target_os = "windows")]
use super::seek_ack_is_current;
#[cfg(target_os = "windows")]
use super::verdict_names_current_stream;
#[cfg(target_os = "windows")]
use crate::player::asio::host::{AsioEvent, AsioHandle};
#[cfg(target_os = "windows")]
use crate::player::thread::DecodeEvent;
#[cfg(target_os = "windows")]
use crate::player::thread::harness::{Harness, Spy, harness, last_state, load_req, load_req_of};
#[cfg(target_os = "windows")]
use crate::player::thread::wav_fixture::wav_s16_mono;
#[cfg(target_os = "windows")]
use crate::player::{PlaybackState, PlayerEvent, ResumePolicy};
#[cfg(target_os = "windows")]
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
#[cfg(target_os = "windows")]
use std::sync::{Arc, mpsc};

/// The teardown used to clear the queued seek whatever track was loading next. A restart
/// contracts to position 0 and discards the seek whatever the tag still holds.
#[cfg(target_os = "windows")]
#[test]
fn a_queued_seek_survives_only_the_re_arm_of_its_own_track() {
    for (queued_track, track_id, resume_policy, expected) in [
        (
            Some("track-1"),
            "track-1",
            ResumePolicy::Explicit(12.0),
            true,
        ),
        (
            Some("track-1"),
            "track-2",
            ResumePolicy::Explicit(12.0),
            false,
        ),
        (Some("track-1"), "track-1", ResumePolicy::Restart, false),
        (None, "track-1", ResumePolicy::Explicit(12.0), false),
    ] {
        assert_eq!(
            queued_seek_survives(queued_track, track_id, resume_policy),
            expected,
            "queued for {queued_track:?}, loading {track_id:?}, {resume_policy:?}"
        );
    }
}

/// A track change mints a new decoder, and its predecessor's answer must not land on it (stream
/// half). On one live decoder only the generation separates the awaited seek from an earlier
/// one still answering, which would clear the newer seek's pin (generation half).
#[cfg(target_os = "windows")]
#[test]
fn only_the_awaited_seeks_own_ack_settles_it() {
    for (current_stream, current_gen, ack_stream, ack_gen, expected) in [
        (Some(8), 3, 7, 3, false),
        (Some(7), 4, 7, 3, false),
        (Some(7), 3, 7, 3, true),
        (None, 3, 7, 3, false),
    ] {
        assert_eq!(
            seek_ack_is_current(current_stream, current_gen, ack_stream, ack_gen),
            expected,
            "current ({current_stream:?}, {current_gen}), ack ({ack_stream}, {ack_gen})"
        );
    }
}

/// A rate-locked device takes seconds to refuse a candidate: a superseded stream's verdict
/// must not mark the track that arrived meanwhile. Two unidentified sides are not the same
/// stream (`None == None` read as a match would mark whatever happened to be playing).
#[cfg(target_os = "windows")]
#[test]
fn a_verdict_condemns_a_track_only_when_both_sides_name_the_current_stream() {
    for (current, verdict, expected) in [
        (Some(2), Some(1), false),
        (Some(2), Some(2), true),
        (None, None, false),
        (Some(2), None, false),
        (None, Some(2), false),
    ] {
        assert_eq!(
            verdict_names_current_stream(current, verdict),
            expected,
            "current {current:?}, verdict {verdict:?}"
        );
    }
}

#[cfg(target_os = "windows")]
use crate::state::current_track_fixture::CurrentTrackSlot;

/// A re-armable track in the process-wide slot. A device switch replays a still-streaming track
/// from it.
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

/// A dead bypass decoder hands the device back, drops the seek sender and keeps the resume point
/// whatever killed it. Only the report differs: an unreadable source is a `MediaError`, a stall
/// is `NetworkLost` and never an error code, which the SDK answers by advancing the queue.
#[cfg(target_os = "windows")]
#[test]
fn a_bypass_decode_failure_frees_the_device_and_keeps_the_resume_point() {
    use crate::player::packet_failure::PacketFailure;
    for (what, cause, error, network_lost) in [
        (
            "an unreadable source",
            PacketFailure::Source,
            "decode packet error: truncated frame",
            false,
        ),
        (
            "a stalled download",
            PacketFailure::Network,
            "decode packet error: no data from the download for 30s",
            true,
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let mut h = harness(dir.path());
        h.player.is_exclusive_mode = true;
        h.player.resume_store.set("track-1", 42.0);

        h.player
            .settle_bypass_decode_failure(error.to_string(), cause);

        let (media_error, lost) = {
            let events = h.events.lock().unwrap();
            (
                events
                    .iter()
                    .any(|ev| matches!(ev, PlayerEvent::MediaError { .. })),
                events
                    .iter()
                    .any(|ev| matches!(ev, PlayerEvent::NetworkLost(_))),
            )
        };
        assert_eq!(
            lost, network_lost,
            "{what}: a stall raises the no-connection banner"
        );
        assert_eq!(
            media_error, !network_lost,
            "{what}: any code the SDK maps advances the queue, which a stall has not earned"
        );
        assert!(
            h.player.exclusive_release_at.is_some(),
            "{what}: the device is owed back whatever killed the decoder"
        );
        assert_eq!(
            h.player.resume_store.get("track-1"),
            Some(42.0),
            "{what}: a dead decoder is not a finished track"
        );
        assert!(!h.player.has_track, "{what}");
        assert!(!h.player.is_playing, "{what}");
        assert!(!h.player.seeking, "{what}");
        assert!(
            h.player.exclusive_seek_tx.is_none(),
            "{what}: the sender points at a receiver the dead decoder dropped"
        );
        assert_eq!(h.player.idle_state, PlaybackState::Stopped, "{what}");
        assert_eq!(
            last_state(&h.events),
            Some(PlaybackState::Stopped),
            "{what}"
        );
    }
}

/// `LoadStarted` lands the moment a load begins, its `Load` only once the fetch resolves, and
/// the pause debounce can elapse between the two. Firing there hands back a device the load is
/// about to reopen and vacates the track slot that load is still filling.
#[cfg(target_os = "windows")]
#[test]
fn a_release_deadline_waits_for_an_in_flight_load() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());
    h.player.is_exclusive_mode = true;
    h.player.is_playing = false;
    h.player.exclusive_release_at =
        Some(std::time::Instant::now() - std::time::Duration::from_millis(1));
    h.player.loading_gen = Some(1);

    h.player.poll_exclusive_events();

    assert!(
        h.player.exclusive_release_at.is_some(),
        "an elapsed deadline is the load's to cancel, never this poll's"
    );
    assert!(
        h.player.has_track,
        "a load in flight still owns the track slot it is filling"
    );

    // The other half of the guard: the same elapsed deadline must still fire once no load is
    // pending. Without this the test would pass just as well against a site that never releases.
    h.player.loading_gen = None;
    h.player.poll_exclusive_events();

    assert!(h.player.exclusive_release_at.is_none());
    assert!(!h.player.has_track);
}

/// The release un-adopts the stream, so the decoder's answer to a seek still in flight dies on
/// the stream identity and never lifts the pin. Left standing it outlives the track: the bar
/// stays frozen at the target and resume writes stay suppressed.
#[cfg(target_os = "windows")]
#[test]
fn a_release_settles_the_seek_it_strands() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());
    h.player.is_exclusive_mode = true;
    h.player.is_playing = false;
    h.player.idle_state = PlaybackState::Paused;
    h.player.seeking = true;
    h.player.seek_target = Some(150.0);
    h.player.seek_wall_start = Some(std::time::Instant::now());
    h.player.exclusive_release_at =
        Some(std::time::Instant::now() - std::time::Duration::from_millis(1));

    h.player.poll_exclusive_events();

    assert!(
        !h.player.seeking,
        "nothing survives the release to answer that seek"
    );
    assert_eq!(h.player.seek_target, None);
    assert!(h.player.seek_wall_start.is_none());
    assert_eq!(
        last_state(&h.events),
        Some(PlaybackState::Paused),
        "handle_seek announced Seeking and its end is owed, release or not"
    );
    assert!(
        h.events.lock().unwrap().iter().any(|ev| matches!(
            ev,
            PlayerEvent::TimeUpdate(t, _) if (*t - 150.0).abs() < f64::EPSILON
        )),
        "played_position_secs still reads pre-seek, so dropping the target walks the bar back"
    );
}

/// Every exclusive refusal takes the render thread down with it, and `is_dead()` reports that
/// clean exit exactly as it reports a panic. Leaving the adoption record standing let the panic
/// gate recover a second time on the very tick the refusal already had, sending two replays that
/// carry the same generation and disagree about whether to resume playing.
#[cfg(target_os = "windows")]
#[test]
fn an_exclusive_refusal_drops_the_adoption_it_recovers_from() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());
    h.player.is_exclusive_mode = true;
    h.player.current_exclusive_stream_id = Some(7);

    h.player.rearm_shared_after_exclusive_failure();

    assert!(
        h.player.current_exclusive_stream_id.is_none(),
        "the panic gate reads this field: a stale Some makes it fire on a stream nobody owns"
    );
}

/// The mirror image, pinned deliberately rather than left to look like an oversight. No ASIO
/// refusal ends the control thread, so a stream adopted after one can be genuinely live; clearing
/// the id here would make its `SeekSettled` fail `seek_ack_is_current` and the seek would never
/// settle for the listener.
#[cfg(target_os = "windows")]
#[test]
fn an_asio_refusal_keeps_an_adoption_that_may_still_be_live() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());
    h.player.is_asio_mode = true;
    h.player.current_asio_stream_id = Some(7);

    h.player.rearm_shared_after_asio_failure();

    assert_eq!(
        h.player.current_asio_stream_id,
        Some(7),
        "the ASIO control thread survives its refusals; a newer stream may already be adopted"
    );
}

#[cfg(target_os = "windows")]
#[test]
fn a_seek_with_no_live_asio_decoder_is_queued_against_the_current_track() {
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

/// A decode failure drops the track identity before the SDK's recovery reload: an untagged seek
/// queued there would reach whichever track loads next. Dropping it is the deliberate half; the
/// silence was not, and nothing will ever make the target the transport already took come true.
#[cfg(target_os = "windows")]
#[test]
fn a_dropped_queued_seek_still_answers_with_the_backend_position() {
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

/// The whole chain a queued seek has to survive, which nothing exercised in one piece: a
/// parked switch, the shared fallback, a refusal, the reap, and the backend that spends it.
#[cfg(target_os = "windows")]
#[test]
fn a_queued_seek_survives_a_parked_switch_through_the_reap() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());
    h.player.is_playing = false;
    // No buffer to reuse: the switch takes its still-streaming arm and spawns no decoder.
    h.player.current_buffer = None;
    h.player.is_asio_mode = true;
    h.player.asio_handle = None;
    h.player.pending_device_switch = Some(("dev-1".to_string(), crate::player::OutputMode::Asio));
    h.player.user_seek_override = Some(("track-1".to_string(), 45.0));
    // A live track the switch can re-arm: with no buffer to reuse, it replays this one.
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

/// Names no endpoint on any machine: an exclusive render thread fails its device lookup and
/// exits. The ASIO control thread opens a driver only on a stream start, which a three-byte
/// buffer never reaches: nothing in these tests opens real hardware.
#[cfg(target_os = "windows")]
const NO_SUCH_DEVICE_A: &str = "tidalunar-test: no such device A";
#[cfg(target_os = "windows")]
const NO_SUCH_DEVICE_B: &str = "tidalunar-test: no such device B";

#[cfg(target_os = "windows")]
fn replay_requested(events: &Spy) -> bool {
    events
        .lock()
        .unwrap()
        .iter()
        .any(|ev| matches!(ev, PlayerEvent::ReplayRequest { .. }))
}

/// A DASH track as the thread holds it: `load_dash` publishes no retained source, and hands over
/// a buffer that is whole in memory from the start.
#[cfg(target_os = "windows")]
fn hold_a_dash_track(h: &mut Harness) -> CurrentTrackSlot {
    h.player.current_buffer = Some(crate::player::buffer::RamBuffer::from_complete(vec![
        1, 2, 3,
    ]));
    CurrentTrackSlot::clear()
}

/// A device pick while a DASH track plays exclusive. TIDAL commits the pick whatever Rust does;
/// a refusal here leaves the UI on a device the audio never moved to.
#[cfg(target_os = "windows")]
#[test]
fn a_dash_track_in_exclusive_moves_to_the_device_picked() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());
    let _slot = hold_a_dash_track(&mut h);
    h.player.is_exclusive_mode = true;
    h.player.current_device_id = Some(NO_SUCH_DEVICE_A.to_string());
    let decoder_on_a = Arc::new(AtomicBool::new(false));
    h.player.exclusive_stream_cancel = Some(decoder_on_a.clone());

    h.player.handle_set_audio_device(
        NO_SUCH_DEVICE_B.to_string(),
        crate::player::OutputMode::Exclusive,
    );

    assert!(
        decoder_on_a.load(Relaxed),
        "the decoder on device A was never retired"
    );
    assert_eq!(
        h.player.current_device_id.as_deref(),
        Some(NO_SUCH_DEVICE_B)
    );
    assert!(h.player.is_exclusive_mode);
    assert!(
        h.player.exclusive_handle.is_some(),
        "no render thread for device B"
    );
    assert!(
        h.player.has_track && h.player.current_exclusive_stream_id.is_some(),
        "the bytes in memory are the whole track: it restarts on them"
    );
    assert!(!replay_requested(&h.events));
}

/// The exclusive toggle while a DASH track plays shared. TIDAL shows exclusive from the moment it
/// sends this. The audio has to follow.
#[cfg(target_os = "windows")]
#[test]
fn a_shared_dash_track_follows_the_exclusive_toggle() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());
    let _slot = hold_a_dash_track(&mut h);

    h.player.handle_set_audio_device(
        NO_SUCH_DEVICE_A.to_string(),
        crate::player::OutputMode::Exclusive,
    );

    assert!(h.player.is_exclusive_mode, "the toggle was refused");
    assert_eq!(
        h.player.current_device_id.as_deref(),
        Some(NO_SUCH_DEVICE_A)
    );
    assert!(h.player.exclusive_handle.is_some());
    assert!(h.player.has_track && h.player.current_exclusive_stream_id.is_some());
    assert!(!replay_requested(&h.events));
}

#[cfg(target_os = "windows")]
#[test]
fn a_shared_dash_track_follows_the_asio_toggle() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());
    let _slot = hold_a_dash_track(&mut h);

    h.player.handle_set_audio_device(
        NO_SUCH_DEVICE_A.to_string(),
        crate::player::OutputMode::Asio,
    );

    assert!(h.player.is_asio_mode, "the toggle was refused");
    assert_eq!(
        h.player.current_device_id.as_deref(),
        Some(NO_SUCH_DEVICE_A)
    );
    assert!(h.player.asio_handle.is_some());
    assert!(h.player.has_track && h.player.current_asio_stream_id.is_some());
    assert!(!replay_requested(&h.events));
}

/// Leaving ASIO drains the driver first, parking the switch for the teardown poll to replay. A
/// DASH track used to be refused before it ever got parked.
#[cfg(target_os = "windows")]
#[test]
fn a_dash_track_in_asio_parks_its_way_back_to_shared() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());
    let _slot = hold_a_dash_track(&mut h);
    let (handle, _driver_events, _driver_cmds) = AsioHandle::for_test();
    h.player.is_asio_mode = true;
    h.player.asio_handle = Some(handle);

    h.player.handle_set_audio_device(
        NO_SUCH_DEVICE_B.to_string(),
        crate::player::OutputMode::Shared,
    );

    assert!(
        h.player.asio_handle.is_none(),
        "the ASIO handle was never torn down"
    );
    assert_eq!(
        h.player.pending_device_switch,
        Some((
            NO_SUCH_DEVICE_B.to_string(),
            crate::player::OutputMode::Shared
        ))
    );
}

#[cfg(target_os = "windows")]
#[test]
fn a_switch_under_an_unlanded_dash_load_leaves_that_load_in_flight() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());
    let _slot = CurrentTrackSlot::clear();
    let (streaming, _writer) = crate::player::buffer::RamBuffer::new_for_test(1_000);
    h.player.current_buffer = Some(streaming);
    h.player.loading_gen = Some(41);
    h.player.pending_play = Some(41);

    h.player.handle_set_audio_device(
        NO_SUCH_DEVICE_A.to_string(),
        crate::player::OutputMode::Exclusive,
    );

    assert!(h.player.is_exclusive_mode, "the toggle was refused");
    assert!(h.player.exclusive_handle.is_some());
    assert!(
        !h.player.has_track,
        "the torn-down track has no backend left"
    );
    assert!(!h.player.is_playing);
    assert_eq!(
        (h.player.loading_gen, h.player.pending_play),
        (Some(41), Some(41)),
        "nothing superseded that load: a play arriving before it lands still waits for it"
    );
    assert!(!replay_requested(&h.events));
}

/// The sibling the case above must not turn into: a still-streaming track with a retained
/// source comes back through a fresh load of it, which owns the load bookkeeping from here.
#[cfg(target_os = "windows")]
#[test]
fn a_switch_on_a_streaming_track_replays_its_retained_source() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());
    let _track = ReplayableTrack::set("https://example.invalid/track-1.flac");
    let (streaming, _writer) = crate::player::buffer::RamBuffer::new_for_test(1_000);
    h.player.current_buffer = Some(streaming);
    h.player.loading_gen = Some(41);

    h.player.handle_set_audio_device(
        NO_SUCH_DEVICE_A.to_string(),
        crate::player::OutputMode::Exclusive,
    );

    assert!(h.player.is_exclusive_mode);
    assert!(!h.player.has_track);
    assert_eq!(
        h.player.loading_gen, None,
        "the replay's own load replaces it"
    );
    assert!(h.events.lock().unwrap().iter().any(|ev| matches!(
        ev,
        PlayerEvent::ReplayRequest { track, play: true, .. }
            if track.url == "https://example.invalid/track-1.flac"
    )));
}

/// Exclusive seeks used to sit outside the seek protocol entirely: no pin, no announcement.
/// The render's pre-seek position walked the bar back, and the frontend had to guess.
#[cfg(target_os = "windows")]
#[test]
fn an_exclusive_seek_pins_and_announces_only_once_dispatched() {
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
    // No handle: the branch is taken and spawns nothing. That is the window under test: the
    // device is not open, and nothing downstream can speak for it until it is.
    h.player.exclusive_handle = None;

    let delivered = h.player.handle_load(load_req(1, "track-1"), false);

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
#[test]
fn an_asio_seek_pins_and_announces_only_once_dispatched() {
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
#[test]
fn a_play_during_an_asio_park_reports_the_real_position() {
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

/// A refusal from a stream already superseded must not reach the track that replaced it (the
/// format half reads the TRACK's channel count); named for the stream still live it keeps every
/// effect, a rate refusal also barring the track from re-engaging ASIO. The exclusive twin
/// re-arms shared on any refusal, and may: no ASIO refusal takes the control thread down.
#[cfg(target_os = "windows")]
#[test]
fn an_asio_refusal_acts_only_for_the_stream_still_live() {
    for (rate, refused_stream, acts, skipped) in [
        (true, 1, false, None),
        (false, 1, false, None),
        (false, 2, true, None),
        (true, 2, true, Some("track-1")),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let (mut h, asio_events, cancel) = asio_harness(dir.path(), 2);
        let stream_id = Some(refused_stream);
        let refusal = if rate {
            AsioEvent::RateUnsupported { stream_id }
        } else {
            AsioEvent::FormatUnsupported { stream_id }
        };

        asio_events.send(refusal).unwrap();
        h.player.poll_asio_events();

        let row = format!("rate: {rate}, refused stream {refused_stream} of live 2");
        assert_eq!(!h.player.is_asio_mode, acts, "ASIO state after {row}");
        assert_eq!(cancel.load(Relaxed), acts, "decoder cancel after {row}");
        if rate {
            assert_eq!(
                h.player.asio_skip_track.as_deref(),
                skipped,
                "skip after {row}"
            );
        }
    }
}

/// The transport states the player has handed its callback, with the seq each travels under.
#[cfg(target_os = "windows")]
fn state_stamps(events: &Spy) -> Vec<(PlaybackState, u32)> {
    events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|ev| match ev {
            PlayerEvent::StateChange(state, seq) => Some((*state, *seq)),
            _ => None,
        })
        .collect()
}

/// Track A's stream is paused by the skip that loads B, and the backend reports it only once
/// the stop and reset of a dirty endpoint are done, after B has loaded and moved the seq. Under
/// B's seq the Connect receiver hears it as B's own; the seq rides with the state whatever order
/// the reports arrive in.
#[cfg(target_os = "windows")]
#[tokio::test]
async fn an_asio_state_keeps_the_seq_of_its_load_in_whatever_order_the_reports_arrive() {
    let _serialised = crate::audio::preload::tests::PRELOAD_TESTS.lock().await;
    for reports in [
        [(1, PlaybackState::Paused), (2, PlaybackState::Active)],
        [(2, PlaybackState::Active), (1, PlaybackState::Paused)],
    ] {
        let dir = tempfile::tempdir().unwrap();
        let (mut h, asio_events, _cancel) = asio_harness(dir.path(), 0);
        let _ = h.player.handle_load(load_req(1, "track-1"), false);
        let _ = h.player.handle_load(load_req(2, "track-2"), false);
        h.events.lock().unwrap().clear();

        for (opened_under, state) in reports {
            asio_events
                .send(AsioEvent::StateChange {
                    opened_under,
                    state,
                })
                .unwrap();
        }
        h.player.poll_asio_events();

        assert_eq!(
            state_stamps(&h.events),
            reports.map(|(seq, state)| (state, seq)).to_vec(),
            "A's pause is B's, or B's own state lost its seq: {reports:?}"
        );
    }
}

#[cfg(target_os = "windows")]
#[tokio::test]
async fn a_retired_exclusive_streams_pause_keeps_the_seq_of_its_load() {
    use crate::player::wasapi::{ExclusiveEvent, ExclusiveHandle};
    let _serialised = crate::audio::preload::tests::PRELOAD_TESTS.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());
    let (handle, exclusive_events, _commands) = ExclusiveHandle::for_test();
    h.player.is_exclusive_mode = true;
    h.player.exclusive_handle = Some(handle);
    let _ = h.player.handle_load(load_req(1, "track-1"), false);
    let _ = h.player.handle_load(load_req(2, "track-2"), false);
    h.events.lock().unwrap().clear();

    exclusive_events
        .send(ExclusiveEvent::StateChange {
            opened_under: 1,
            state: PlaybackState::Paused,
        })
        .unwrap();
    exclusive_events
        .send(ExclusiveEvent::StateChange {
            opened_under: 2,
            state: PlaybackState::Active,
        })
        .unwrap();
    h.player.poll_exclusive_events();

    assert_eq!(
        state_stamps(&h.events),
        vec![(PlaybackState::Paused, 1), (PlaybackState::Active, 2)],
        "A's pause is B's, or B's own state lost its seq"
    );
}

/// A stop keeps the pipeline for a same-track re-assert; the resume that follows it is
/// answered by the very stream the stop left in place. The SDK drops anything below the newest
/// seq it has heard, the stop's included: stamped with the load's, that `active` would never
/// reach it.
#[cfg(target_os = "windows")]
#[tokio::test]
async fn a_stop_leaves_the_stream_it_keeps_answering_under_the_newest_seq() {
    let _serialised = crate::audio::preload::tests::PRELOAD_TESTS.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let (mut h, asio_events, _cancel) = asio_harness(dir.path(), 0);
    let _ = h.player.handle_load(load_req(1, "track-1"), false);
    h.player.handle_stop(3);
    h.events.lock().unwrap().clear();

    asio_events
        .send(AsioEvent::StateChange {
            opened_under: 1,
            state: PlaybackState::Active,
        })
        .unwrap();
    h.player.poll_asio_events();

    assert_eq!(
        state_stamps(&h.events),
        vec![(PlaybackState::Active, 3)],
        "a resume after a stop travelled under the seq the SDK already moved past"
    );
}

/// The player names the load on the stream's own `StartStream`, which is where every state the
/// backend reports for it takes its seq from.
#[cfg(target_os = "windows")]
#[tokio::test]
async fn an_asio_stream_is_opened_under_the_seq_of_the_load_that_spawned_it() {
    use crate::player::asio::host::AsioCommand;
    let _serialised = crate::audio::preload::tests::PRELOAD_TESTS.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());
    let (handle, _events, commands) = AsioHandle::for_test();
    h.player.is_asio_mode = true;
    h.player.asio_handle = Some(handle);
    let wav = wav_s16_mono(44_100, &[1, 2, 3, 4]);
    let _ = h.player.handle_load(
        load_req_of(
            crate::player::buffer::RamBuffer::from_complete(wav),
            7,
            "track-7",
        ),
        false,
    );

    match commands.recv() {
        Ok(AsioCommand::StartStream { opened_under, .. }) => assert_eq!(opened_under, 7),
        _ => panic!("the decoder's first word was not the StartStream of the load"),
    }
}

#[cfg(target_os = "windows")]
#[tokio::test]
async fn an_exclusive_stream_is_opened_under_the_seq_of_the_load_that_spawned_it() {
    use crate::player::wasapi::{ExclusiveCommand, ExclusiveHandle};
    let _serialised = crate::audio::preload::tests::PRELOAD_TESTS.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());
    let (handle, _events, commands) = ExclusiveHandle::for_test();
    h.player.is_exclusive_mode = true;
    h.player.exclusive_handle = Some(handle);
    let wav = wav_s16_mono(44_100, &[1, 2, 3, 4]);
    let _ = h.player.handle_load(
        load_req_of(
            crate::player::buffer::RamBuffer::from_complete(wav),
            7,
            "track-7",
        ),
        false,
    );

    match commands.recv() {
        Ok(ExclusiveCommand::StartStream(start)) => assert_eq!(start.opened_under, 7),
        _ => panic!("the decoder's first word was not the StartStream of the load"),
    }
}
