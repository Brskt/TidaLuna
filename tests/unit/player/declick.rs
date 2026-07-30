//! Tests for `src/player/declick.rs`, attached to it by `#[path]`.

use super::*;

#[test]
fn silence_frames_scales_with_rate() {
    assert_eq!(silence_frames(48_000, 200.0), 9_600);
    assert_eq!(silence_frames(44_100, 10.0), 441);
    assert_eq!(silence_frames(0, 200.0), 0);
}

#[test]
fn fade_in_runs_zero_to_one() {
    assert!(fade_in_env(0, 441).abs() < 1e-6);
    assert!((fade_in_env(441, 441) - 1.0).abs() < 1e-6);
    // Monotone rising and bounded.
    let (a, b) = (fade_in_env(110, 441), fade_in_env(330, 441));
    assert!(a < b && (0.0..=1.0).contains(&a) && (0.0..=1.0).contains(&b));
}

#[test]
fn fade_out_runs_one_to_zero() {
    assert!((fade_out_env(0, 441) - 1.0).abs() < 1e-6);
    assert!(fade_out_env(441, 441).abs() < 1e-6);
    // Fade-in and fade-out are complementary at every point.
    for p in [0, 100, 220, 441] {
        assert!((fade_in_env(p, 441) + fade_out_env(p, 441) - 1.0).abs() < 1e-6);
    }
}

#[test]
fn zero_len_does_not_divide_by_zero() {
    assert!(fade_in_env(0, 0).is_finite());
    assert!(fade_out_env(0, 0).is_finite());
}

#[test]
fn fade_scale_attenuates() {
    assert_eq!(fade_scale(1000, 0.0), 0);
    assert_eq!(fade_scale(1000, 1.0), 1000);
    assert_eq!(fade_scale(1000, 0.5), 500);
}

#[test]
fn fade_out_wait_scales_with_buffer_period() {
    let fade = silence_frames(44_100, DECLICK_FADE_MS); // 441 frames
    // Small buffers: (1 ramp period + 3) * 11ms sits under the floor.
    assert_eq!(fade_out_wait_ms(512, 44_100, fade), FADE_OUT_WAIT_MS);
    // Mid-size buffers land between floor and cap: 4 * (2048000/44100)ms.
    assert_eq!(fade_out_wait_ms(2048, 44_100, fade), 184);
    assert_eq!(
        fade_out_wait_ms(2048, 48_000, silence_frames(48_000, DECLICK_FADE_MS)),
        168
    );
    assert_eq!(
        fade_out_wait_ms(2048, 96_000, silence_frames(96_000, DECLICK_FADE_MS)),
        84
    );
    // Huge buffers hit the cap.
    assert_eq!(fade_out_wait_ms(8192, 44_100, fade), FADE_OUT_WAIT_MAX_MS);
}

#[test]
fn fade_out_wait_survives_degenerate_inputs() {
    // Zero rate: the per-period estimate falls back to the floor constant.
    assert_eq!(fade_out_wait_ms(512, 0, 441), 4 * FADE_OUT_WAIT_MS);
    // Zero frames / zero fade never divide by zero and stay clamped in range.
    let w = fade_out_wait_ms(0, 48_000, 0);
    assert!((FADE_OUT_WAIT_MS..=FADE_OUT_WAIT_MAX_MS).contains(&w));
}
