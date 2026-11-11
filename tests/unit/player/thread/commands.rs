//! Tests for `src/player/thread/commands.rs`, attached to it by `#[path]`. The player of the
//! shared `harness` fixture also drives `poll_playback`, which lives in the sibling `playback`
//! module: both are `pub(in player::thread)`: either file reaches the whole surface.

#[cfg(target_os = "windows")]
use super::queued_seek_survives;
use super::{LoadRequest, PlayAction, ResumePolicy, decide_play, settle_load};
#[cfg(target_os = "windows")]
use crate::player::asio::host::{AsioEvent, AsioHandle};
#[cfg(target_os = "windows")]
use crate::player::thread::harness::Harness;
use crate::player::thread::harness::{Callback, Spy, harness};
#[cfg(target_os = "windows")]
use crate::player::thread::wav_fixture::wav_s16_mono;
use crate::player::thread::{DecodeEvent, PlayerThread};
use crate::player::{PlaybackState, PlayerCommand, PlayerEvent};
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

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

/// A stale settle must not clear a newer in-flight load. A load that failed delivered no
/// `handle_load`; the play deferred on its generation must not dangle either.
#[test]
fn a_settle_clears_only_what_waits_on_its_own_generation() {
    for (loading_gen, pending_play, generation, expected) in [
        (Some(6), None, 5, (Some(6), None)),
        (Some(5), Some(5), 5, (None, None)),
        (None, Some(9), 5, (None, Some(9))),
    ] {
        assert_eq!(
            settle_load(loading_gen, pending_play, generation),
            expected,
            "settle_load({loading_gen:?}, {pending_play:?}, {generation})"
        );
    }
}

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
            cached: Some(crate::player::cache::Generation::NONE),
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

    let delivered = h.player.handle_load(load_req(3, "track-2"), false);

    assert!(!delivered, "three bytes cannot probe as audio");
    assert!(
        !h.player.has_track,
        "a failed load left a track flag that nothing backs"
    );
}

/// The id and seq stamped on the last media error, the outer `Option` saying whether one was raised.
fn last_media_error(events: &Spy) -> Option<(u32, Option<String>)> {
    events.lock().unwrap().iter().rev().find_map(|ev| match ev {
        PlayerEvent::MediaError {
            seq, product_id, ..
        } => Some((*seq, product_id.clone())),
        _ => None,
    })
}

/// The renderer refuses a track it loaded itself on the name its failure carries. A probe
/// failure must name the load that failed, not the track the thread held before it.
#[tokio::test]
async fn a_probe_failure_names_the_track_its_load_carried() {
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
            cached: Some(crate::player::cache::Generation::NONE),
            format: "flac".to_string(),
        },
        false,
    );

    assert!(!delivered, "three bytes cannot probe as audio");
    assert_eq!(
        last_media_error(&h.events),
        Some((7, Some("120002099".to_string())))
    );
}

/// A fetch that failed never reached `handle_load`: the thread still holds the previous track,
/// and the id has to come from the command. A load a newer one superseded reports nothing (the
/// renderer is never told that a track it has since replaced failed).
#[tokio::test]
async fn a_fetch_failure_names_the_track_its_load_carried_unless_superseded() {
    let _serialised = crate::audio::preload::tests::PRELOAD_TESTS.lock().await;
    for (generations_behind, expected) in
        [(0u32, Some((9, Some("120002099".to_string())))), (1, None)]
    {
        let dir = tempfile::tempdir().unwrap();
        let mut h = harness(dir.path());
        h.player.current_product_id = Some("previous".to_string());

        h.player.handle_command(PlayerCommand::LoadFailed {
            error: "HTTP 403 Forbidden".to_string(),
            code: crate::player::MediaErrorCode::NoSuchFile,
            seq: 9,
            load_gen: crate::player::current_gen().wrapping_sub(generations_behind),
            product_id: Some("120002099".to_string()),
        });

        assert_eq!(
            last_media_error(&h.events),
            expected,
            "a load {generations_behind} generations behind"
        );
    }
}

/// The re-assert is the one load path that never reaches `handle_load`; it is the only one that
/// has to carry the id itself. Dropping it there left a gapless-advanced track nameless for the
/// rest of its life, with every length it measured publishable under nothing.
#[test]
fn a_re_assert_brings_the_track_its_load_named() {
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
#[test]
fn a_re_assert_without_an_id_keeps_the_one_already_known() {
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

/// The re-assert is minted from a `committed_track` read on the caller's thread, and a load for
/// another track can commit between the mint and the handling. Applying the id then would rename
/// the arriving track after the one that just left, and every length it measures along with it.
#[test]
fn a_re_assert_for_a_superseded_track_keeps_the_live_id() {
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

/// Both candidates compete for one slot: taking only the winner leaves the loser armed for the
/// next read. A load announces where it will open before the backend is chosen: the
/// announced position has to be the one the stream opens at.
#[cfg(target_os = "windows")]
#[test]
fn the_start_position_is_the_queued_seek_else_the_auto_resume_and_taking_retires_both() {
    for (queued, auto_resume, expected) in [
        (Some(10.0), Some(45.0), Some(10.0)),
        (None, Some(45.0), Some(45.0)),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let mut h = harness(dir.path());
        h.player.user_seek_override = queued.map(|time| ("track-1".to_string(), time));
        h.player.pending_resume_seek = auto_resume;

        assert_eq!(
            h.player.start_position(),
            expected,
            "announced, queued {queued:?} auto-resume {auto_resume:?}"
        );
        assert_eq!(
            h.player.take_start_position(),
            expected,
            "opened at, queued {queued:?} auto-resume {auto_resume:?}"
        );
        assert_eq!(
            h.player.pending_resume_seek, None,
            "left armed, the next play seeks back to the auto-resume: queued {queued:?}"
        );
        assert!(h.player.user_seek_override.is_none(), "queued {queued:?}");
    }
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

/// A refused seek moved nothing, and decoding runs a ring ahead of the speakers. Persisting
/// what the decoder reported would store that lead as if it had been heard, and rebasing the
/// played counter on it would teleport the position by the same amount.
#[test]
fn a_refused_seek_persists_what_was_played_not_what_was_decoded() {
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

/// The tick that reports a landing returns early on a paused stream: the settle has to carry
/// the position itself. Without it the bar keeps the target `handle_seek` announced, for as long
/// as the pause lasts. Both bypass backends already answer this way.
#[test]
fn a_seek_answered_while_paused_reports_the_landing() {
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
#[test]
fn stopping_the_decoder_retires_its_reader() {
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

/// A failed load clears the flag every position emitter keys on; the fields describing that
/// position have to fall with it. The seek answer is the shortest path to the consequence: it
/// would otherwise carry the replaced track's position under the failed load's seq.
#[test]
fn a_failed_load_leaves_no_position_from_the_track_it_replaced() {
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
#[test]
fn a_refused_pre_seek_is_not_announced_as_the_start_position() {
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

/// Two seeks in flight: `SeekComplete` used to carry no identity, and the stale first answer
/// could settle the pin and persist its own position over the seek still being awaited.
#[test]
fn a_superseded_seek_answer_settles_nothing() {
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

#[test]
fn a_seek_settled_after_a_stop_keeps_the_stopped_state() {
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

#[test]
fn a_seek_queued_behind_a_load_persists_nothing() {
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

/// A fatal decode error nulled the sender while a newer load is still fetching. The IPC handler
/// flushed the seek target to the frontend before the thread ever saw it, and that load's own
/// `handle_load` overwrites the queued target before any decoder reads it: the queue cannot be
/// what answers. The counter is set apart from the target, keeping the answer from being a constant.
#[test]
fn a_seek_queued_behind_a_load_answers_the_played_position() {
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

#[test]
fn a_seek_onto_a_dead_pipeline_puts_the_real_position_back() {
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

#[test]
fn a_seek_settled_before_the_first_play_announces_ready() {
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

#[test]
fn a_seek_settled_after_a_pause_before_the_first_play_keeps_paused() {
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

#[test]
fn a_decode_error_while_paused_reaches_a_terminal_state() {
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

#[test]
fn a_seek_in_flight_holds_off_the_drain() {
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

#[test]
fn retiring_the_decoder_clears_a_pending_seek() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());

    h.player.handle_seek(30.0);
    assert!(h.player.seeking);

    h.player.stop_decode();

    assert!(!h.player.seeking, "no decoder is left to answer that seek");
    assert_eq!(h.player.seek_target, None);
}

#[test]
fn a_fatal_error_during_a_seek_settles_instead_of_stranding_it() {
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

#[test]
fn a_seek_after_a_deferred_decode_error_still_lets_the_track_finish() {
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

#[test]
fn a_completed_track_leaves_no_decoder_behind() {
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

#[test]
fn a_seek_reviving_a_parked_decoder_disarms_the_pending_completion() {
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

#[test]
fn decode_reaching_eof_while_paused_keeps_the_resume_position() {
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

fn load_req_of(buffer: crate::player::buffer::RamBuffer, seq: u32, track: &str) -> LoadRequest {
    LoadRequest {
        buffer,
        load_gen: crate::player::current_gen(),
        seq,
        track_id: track.to_string(),
        product_id: None,
        resume_policy: ResumePolicy::Restart,
        load_start: std::time::Instant::now(),
        cached: Some(crate::player::cache::Generation::NONE),
        format: "flac".to_string(),
    }
}

/// Bytes no decoder reads: the stream is opened and named, which is all these cases need.
fn load_req(seq: u32, track: &str) -> LoadRequest {
    load_req_of(
        crate::player::buffer::RamBuffer::from_complete(vec![1, 2, 3]),
        seq,
        track,
    )
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

/// A receiver session started before any page plays at the level the last launch left, not at
/// full scale: the thread's gain starts there.
#[test]
fn the_thread_starts_at_the_restored_level() {
    let (_cmd_tx, cmd_rx) = mpsc::channel();
    let callback: Callback = Box::new(|_| {});
    let restored = crate::player::Volume::from_percent(40.0);
    let player = PlayerThread::new(
        cmd_rx,
        callback,
        false,
        Some(restored),
        Arc::new(Mutex::new(None)),
        crate::audio::bandwidth::GovernorHandle::ungoverned(),
    )
    .expect("construction opens no device and cannot fail off Windows");
    assert_eq!(f32::from_bits(player.volume.load(Relaxed)), 0.4);
    // On Windows the restored level also seeds the OS session and the exclusive/ASIO gain.
    #[cfg(target_os = "windows")]
    {
        assert!(player.volume_baseline_established);
        assert_eq!(f32::from_bits(player.last_volume.load(Relaxed)), 0.4);
        assert_eq!(f32::from_bits(player.exclusive_gain.load(Relaxed)), 0.4);
    }
}

/// A page that ends stops its track, which stays retained and resumable. The page that follows
/// presses Play for the track ITS queue names, and a resume of the retained one would play a track
/// nothing on screen names until that page's own load replaced it.
#[test]
fn a_play_after_the_page_that_loaded_the_track_ended_resumes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());

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
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());

    h.player.handle_command(PlayerCommand::Stop(1));
    h.player.handle_play();

    assert!(h.player.is_playing);
    assert_eq!(last_state(&h.events), Some(PlaybackState::Active));
}

/// The intent a stop records for a quality swap's re-assert is the ended page's: a page that
/// loads the same track on boot finds it stopped, as a reload leaves everything.
#[test]
fn the_page_that_follows_finds_the_retained_track_stopped_not_to_be_resumed() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());

    h.player.handle_command(PlayerCommand::Stop(1));
    assert!(
        h.player.resume_on_reassert,
        "a stop of a playing track records the intent"
    );
    h.player.handle_command(PlayerCommand::Orphan);

    assert!(!h.player.resume_on_reassert);
}

/// After a load names a track again, a Play is a Play of that track; a load a newer one
/// superseded names nothing, and leaves the track as orphaned as it was.
#[tokio::test]
async fn a_load_lifts_the_orphaning_unless_a_newer_one_superseded_it() {
    let _serialised = crate::audio::preload::tests::PRELOAD_TESTS.lock().await;
    for (generations_behind, orphaned) in [(0u32, false), (1, true)] {
        let dir = tempfile::tempdir().unwrap();
        let mut h = harness(dir.path());
        h.player.handle_command(PlayerCommand::Stop(1));
        h.player.handle_command(PlayerCommand::Orphan);

        h.player.handle_load(
            LoadRequest {
                load_gen: crate::player::current_gen().wrapping_sub(generations_behind),
                ..load_req(2, "track-2")
            },
            false,
        );

        assert_eq!(
            h.player.orphaned, orphaned,
            "a load {generations_behind} generations behind"
        );
    }
}

/// A page that loads the retained track again, by the idempotent branch of `Player::load`, names
/// it: the re-assert lifts the orphaning and, the page being new, resumes nothing by itself. One
/// that names another track than the committed one names nothing of this one's.
#[test]
fn a_reassert_lifts_the_orphaning_only_for_the_committed_track() {
    for (track_id, orphaned) in [("track-1", false), ("another", true)] {
        let dir = tempfile::tempdir().unwrap();
        let mut h = harness(dir.path());
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
