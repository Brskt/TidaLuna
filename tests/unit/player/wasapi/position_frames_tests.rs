//! Tests for `src/player/wasapi.rs`, attached to it by `#[path]`.

use super::position_frames;

#[test]
fn maps_seconds_to_frames() {
    assert_eq!(position_frames(62.4, 44100), 2_751_840);
    assert_eq!(position_frames(0.0, 44100), 0);
    assert_eq!(position_frames(-1.0, 44100), 0);
    assert_eq!(position_frames(10.0, 0), 0);
    assert_eq!(position_frames(f64::NAN, 44100), 0);
}
