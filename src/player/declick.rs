//! Shared de-click / resync primitives for the Windows audio backends (ASIO + WASAPI
//! exclusive). Both restart the device on a sample-rate change, which pops the DAC; these
//! helpers fade the stream edges and size the post-restart silence that masks the hardware
//! PLL relock. Platform-independent, so the envelope math is unit-tested on any host.
#![cfg_attr(not(target_os = "windows"), allow(dead_code))]

/// De-click fade length: teardown fade-out and post-resync fade-in. Short enough
/// to be musically imperceptible, long enough to guarantee a clean zero crossing.
pub(crate) const DECLICK_FADE_MS: f64 = 10.0;
/// Silence held after the device restarts at the new rate to mask the DAC PLL
/// relock; sized to cover RME's single-speed firmware mute window. The track head
/// is delayed, not dropped (the backend does not consume PCM during it).
pub(crate) const RESYNC_SILENCE_MS: f64 = 200.0;
/// Floor and cap for the fade-out deadline before a format-change teardown (ASIO): the
/// control thread polls `fade_out_done` between commands and rebuilds when the RT signals
/// it or this bound elapses (a stuck RT must not wedge the rebuild forever).
pub(crate) const FADE_OUT_WAIT_MS: u64 = 80;
pub(crate) const FADE_OUT_WAIT_MAX_MS: u64 = 500;

/// Frames spanning `ms` at `sample_rate` Hz.
#[inline]
pub(crate) fn silence_frames(sample_rate: u32, ms: f64) -> usize {
    (sample_rate as f64 * ms / 1000.0) as usize
}

/// Rising half-cosine fade-in coefficient at frame `pos` of `len` (0.0 -> 1.0). The
/// derivative is zero at both ends, so the ramp adds no click of its own.
#[inline]
pub(crate) fn fade_in_env(pos: usize, len: usize) -> f32 {
    0.5 * (1.0 - (std::f32::consts::PI * pos as f32 / len.max(1) as f32).cos())
}

/// Descending half-cosine fade-out coefficient at frame `pos` of `len` (1.0 -> 0.0).
#[inline]
pub(crate) fn fade_out_env(pos: usize, len: usize) -> f32 {
    0.5 * (1.0 + (std::f32::consts::PI * pos as f32 / len.max(1) as f32).cos())
}

/// Scale a right-justified i32 PCM sample by a fade envelope in `[0.0, 1.0]`.
#[inline]
pub(crate) fn fade_scale(sample: i32, env: f32) -> i32 {
    (sample as f32 * env) as i32
}

/// Milliseconds the control thread allows the RT fade-out before tearing the stream
/// down anyway. `fade_out_done` lands two silent fills after the ramp (ASIO plays one
/// buffer ahead), so cover the ramp's buffer periods plus 3 of slack, clamped to
/// [`FADE_OUT_WAIT_MS`, `FADE_OUT_WAIT_MAX_MS`]. `frames`/`sample_rate` describe the
/// OLD (fading) stream.
#[inline]
pub(crate) fn fade_out_wait_ms(frames: usize, sample_rate: u32, fade_len: usize) -> u64 {
    let period_ms = (frames as u64 * 1000)
        .checked_div(sample_rate as u64)
        .unwrap_or(FADE_OUT_WAIT_MS);
    let fade_periods = fade_len.div_ceil(frames.max(1)).max(1) as u64;
    ((fade_periods + 3) * period_ms).clamp(FADE_OUT_WAIT_MS, FADE_OUT_WAIT_MAX_MS)
}

#[cfg(test)]
mod tests {
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
}
