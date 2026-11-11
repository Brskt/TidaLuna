//! Tests for `src/player/thread/output/stream.rs`, attached to it by `#[path]`.

use super::attempt_order;

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
