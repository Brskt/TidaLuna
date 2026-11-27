//! Tests for `src/player/thread/output/stream.rs`, attached to it by `#[path]`.

use super::{StreamFault, attempt_order, record_stream_fault};
use crate::player::thread::output::{
    STREAM_ERR_DEFAULT_GONE, STREAM_ERR_DEVICE_LOST, STREAM_ERR_NONE, STREAM_ERR_UNKNOWN,
};
use std::sync::atomic::AtomicU8;
use std::sync::atomic::Ordering::Relaxed;

/// The `Other` that follows a lost default right after must not turn the loss into an unknown
/// error: the page would show NPD-unknown instead of NPD01.
#[test]
fn a_weaker_stream_fault_never_replaces_a_stronger_one() {
    let flag = AtomicU8::new(STREAM_ERR_NONE);
    record_stream_fault(&flag, StreamFault::DefaultGone);
    record_stream_fault(&flag, StreamFault::Unknown);
    assert_eq!(flag.load(Relaxed), STREAM_ERR_DEFAULT_GONE);
    record_stream_fault(&flag, StreamFault::Lost);
    record_stream_fault(&flag, StreamFault::DefaultGone);
    assert_eq!(flag.load(Relaxed), STREAM_ERR_DEVICE_LOST);

    let flag = AtomicU8::new(STREAM_ERR_NONE);
    record_stream_fault(&flag, StreamFault::Unknown);
    assert_eq!(flag.load(Relaxed), STREAM_ERR_UNKNOWN);
}

/// The order the stream is opened in decides who resamples. On Windows shared mode and
/// on macOS the device's default IS the rate the audio server runs at; targeting it
/// leaves the server nothing to convert. On Linux cpal cannot learn PipeWire's real
/// graph rate (it reports 48000 from two unrelated heuristics), and aiming at it would
/// stack our conversion on top of PipeWire's. `attempt_order` takes that as a plain
/// `pinned` argument instead of reading `ENGINE_RATE_IS_PINNED` itself, leaving both
/// orders asserted here on every host. Either order keeps the other candidate as the
/// fallback, for a device that refuses its own advertised default.
#[test]
fn the_device_default_is_tried_first_only_where_it_means_something() {
    let source = (44_100u32, 2u16);
    let device = (192_000u32, 2u16);

    assert_eq!(
        attempt_order(source, device, true),
        [device, source],
        "the device's own rate has to win"
    );
    assert_eq!(
        attempt_order(source, device, false),
        [source, device],
        "Linux keeps the source-first policy"
    );
}
