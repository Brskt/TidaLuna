//! The player under test shared by the sidecars of `src/player/thread/`, attached once to
//! `thread/mod.rs` by `#[path]`, as `wav_fixture` is.

use crate::player::resume::ResumeStore;
use crate::player::thread::{DecodeCommand, DecodeEvent, PlayerThread};
use crate::player::{LoadRequest, PlaybackState, PlayerCommand, PlayerEvent, ResumePolicy};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

// The transport methods are plain `&mut self` functions. A test drives them directly:
// no run loop, no audio device, no decode thread. The decode thread's reports arrive on a
// channel the test owns, and the governor is one no other test writes to.

pub(super) type Callback = Box<dyn Fn(PlayerEvent) + Send>;
pub(super) type Spy = Arc<Mutex<Vec<PlayerEvent>>>;

pub(super) struct Harness {
    pub(super) player: PlayerThread<Callback>,
    pub(super) events: Spy,
    /// Stands in for the decode thread's event sender.
    pub(super) decode_events: mpsc::Sender<DecodeEvent>,
    /// Both are held for the test's lifetime, keeping the receivers the player owns from
    /// reporting a hung-up channel.
    _cmd_tx: mpsc::Sender<PlayerCommand>,
    _decode_cmds: mpsc::Receiver<DecodeCommand>,
}

/// A player holding one loaded, playing shared-path track. `dir` owns the resume file:
/// no ordering or timing of the calls below can reach the one the running app writes.
pub(super) fn harness(dir: &std::path::Path) -> Harness {
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let events: Spy = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let callback: Callback = Box::new(move |ev| sink.lock().unwrap().push(ev));
    let mut player = PlayerThread::new(
        cmd_rx,
        callback,
        false,
        None,
        Arc::new(Mutex::new(None)),
        crate::audio::bandwidth::GovernorHandle::ungoverned(),
    )
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
pub(super) fn last_state(events: &Spy) -> Option<PlaybackState> {
    events.lock().unwrap().iter().rev().find_map(|ev| match ev {
        PlayerEvent::StateChange(state, _) => Some(*state),
        _ => None,
    })
}

pub(super) fn load_req_of(
    buffer: crate::player::buffer::RamBuffer,
    seq: u32,
    track: &str,
) -> LoadRequest {
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
pub(super) fn load_req(seq: u32, track: &str) -> LoadRequest {
    load_req_of(
        crate::player::buffer::RamBuffer::from_complete(vec![1, 2, 3]),
        seq,
        track,
    )
}
