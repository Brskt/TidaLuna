//! Tests for `src/player/thread/volume.rs`, attached to it by `#[path]`.

use crate::player::thread::PlayerThread;
use crate::player::thread::harness::Callback;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

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
