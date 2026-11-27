//! Tests for `src/player/thread/load.rs`, attached to it by `#[path]`. The player under test
//! comes from the `harness` fixture the sidecars of `thread/` share.

use super::settle_load;
use crate::player::thread::harness::{Spy, harness, load_req, local_stop};
use crate::player::{LoadRequest, PlaybackState, PlayerCommand, PlayerEvent, ResumePolicy};
use std::sync::atomic::Ordering::Relaxed;

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
    // Held for the reason given in the `load_state` tests: this samples the process-wide generation
    // and `handle_load` reads it again; a mint from another test landing between the two
    // makes the load read as stale and ignored.
    let _serialised = crate::audio::preload::tests::PRELOAD_TESTS.lock().await;
    let mut h = harness();

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
            gains: crate::player::normalization::TrackGains::UNITY,
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
/// failure must name the load that failed, not the track the thread held before it. Then the
/// SDK's load is settled with a length of 0 and the track ends: left at Playing, the OS controls
/// and a phone go on showing a track that is gone.
#[tokio::test]
async fn a_probe_failure_names_its_load_and_ends_the_track() {
    let _serialised = crate::audio::preload::tests::PRELOAD_TESTS.lock().await;
    let mut h = harness();
    // What the track this load replaces left standing.
    h.player.is_playing = true;

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
            gains: crate::player::normalization::TrackGains::UNITY,
        },
        false,
    );

    assert!(!delivered, "three bytes cannot probe as audio");
    assert_eq!(
        last_media_error(&h.events),
        Some((7, Some("120002099".to_string())))
    );
    let events = h.events.lock().unwrap();
    let from_error = events
        .iter()
        .position(|ev| matches!(ev, PlayerEvent::MediaError { .. }))
        .expect("the failure is reported");
    assert!(
        matches!(
            &events[from_error + 1..],
            [
                PlayerEvent::Duration(d, 7, Some(p)),
                PlayerEvent::StateChange(PlaybackState::Stopped, 7),
            ] if *d == 0.0 && p == "120002099"
        ),
        "the error, then the length the SDK awaits, then the end: {:?}",
        &events[from_error..]
    );
    assert!(!h.player.has_track && !h.player.is_playing);
    assert!(h.player.current_track_id.is_none());
    assert_eq!(h.player.idle_state, PlaybackState::Stopped);
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
        let mut h = harness();
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
    let mut h = harness();
    h.player.current_duration = 214.0;

    h.player.handle_command(PlayerCommand::ReassertResume {
        want_play: false,
        product_id: Some("120002099".to_string()),
        track_id: "track-1".to_string(),
        read_gen: crate::player::current_gen(),
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
    let mut h = harness();
    h.player.current_duration = 214.0;
    h.player.current_product_id = Some("88264189".to_string());

    h.player.handle_command(PlayerCommand::ReassertResume {
        want_play: false,
        product_id: None,
        track_id: "track-1".to_string(),
        read_gen: crate::player::current_gen(),
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
    let mut h = harness();
    h.player.current_duration = 214.0;
    // What the thread committed to while the re-assert for `track-1` was still in flight.
    h.player.current_track_id = Some("track-2".to_string());
    h.player.current_product_id = Some("the-live-one".to_string());

    h.player.handle_command(PlayerCommand::ReassertResume {
        want_play: false,
        product_id: Some("the-superseded-one".to_string()),
        track_id: "track-1".to_string(),
        read_gen: crate::player::current_gen(),
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

const A_URL: &str = "https://example.invalid/track-a?sig=old";
const A_ID: &str = "https://example.invalid/track-a";

fn track_a() -> crate::state::TrackInfo {
    crate::state::TrackInfo {
        url: A_URL.to_string(),
        key: String::new(),
        format: "flac".to_string(),
        product_id: Some("a-product".to_string()),
        gains: crate::player::normalization::TrackGains::UNITY,
    }
}

/// The thread holding A as a load committed it: playing, announced, mirrored for the IPC side, and
/// retained under `commit_gen` the way `load_with_policy` publishes it.
fn playing_a(
    h: &mut crate::player::thread::harness::Harness,
    commit_gen: u32,
) -> crate::state::current_track_fixture::CurrentTrackSlot {
    let slot = crate::state::current_track_fixture::CurrentTrackSlot::holding(
        crate::state::RetainedTrack {
            track: track_a(),
            load_gen: commit_gen,
        },
    );
    h.player.current_track_id = Some(A_ID.to_string());
    h.player.current_duration = 214.0;
    h.player.current_product_id = Some("a-product".to_string());
    h.player
        .set_committed_track(Some((A_ID.to_string(), "flac".to_string())));
    slot
}

fn reassert(h: &mut crate::player::thread::harness::Harness, want_play: bool, read_gen: u32) {
    h.player.handle_command(PlayerCommand::ReassertResume {
        want_play,
        product_id: Some("a-product".to_string()),
        track_id: A_ID.to_string(),
        read_gen,
    });
}

/// A same-track re-assert dequeued after the Release of that track still serves the page: a
/// replay that expects the generation the page's load read, playing, from where A was last heard.
#[tokio::test]
async fn a_same_track_re_assert_dequeued_after_a_release_replays_the_track() {
    // Mints the process-wide generation: held like every test that does (see `load_state`).
    let _serialised = crate::audio::preload::tests::PRELOAD_TESTS.lock().await;
    let mut h = harness();
    let gen_commit = crate::player::begin_load(crate::player::LoadOrigin::Local);
    let _slot = playing_a(&mut h, gen_commit);
    h.player.resume_store.set(A_ID, 42.0);

    // `Player::release` on the IPC thread: the mint, then the command.
    let gen_release = crate::player::begin_load(crate::player::LoadOrigin::Local);
    h.player.handle_release(crate::player::next_event_seq());
    h.events.lock().unwrap().clear();
    // The page's load of A, read after the release minted.
    reassert(&mut h, true, gen_release);

    let events = h.events.lock().unwrap();
    assert!(
        events.iter().any(|ev| matches!(
            ev,
            PlayerEvent::ReplayRequest { expected_gen, position: Some(p), play: true, .. }
                if *expected_gen == gen_release && *p == 42.0
        )),
        "no live replay of A from its last position: {events:?}"
    );
}

/// A failed load clears the flag every position emitter keys on; the fields describing that
/// position have to fall with it. The seek answer is the shortest path to the consequence: it
/// would otherwise carry the replaced track's position under the failed load's seq.
#[test]
fn a_failed_load_leaves_no_position_from_the_track_it_replaced() {
    let mut h = harness();
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

/// After a load names a track again, a Play is a Play of that track; a load a newer one
/// superseded names nothing, and leaves the track as orphaned as it was.
#[tokio::test]
async fn a_load_lifts_the_orphaning_unless_a_newer_one_superseded_it() {
    let _serialised = crate::audio::preload::tests::PRELOAD_TESTS.lock().await;
    for (generations_behind, orphaned) in [(0u32, false), (1, true)] {
        let mut h = harness();
        h.player
            .handle_command(PlayerCommand::Stop(1, local_stop()));
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
        let mut h = harness();
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
