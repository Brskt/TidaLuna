//! Shared de-click / resync primitives for the Windows audio backends (ASIO + WASAPI
//! exclusive). Both restart the device on a sample-rate change, which pops the DAC; these
//! helpers fade the stream edges and size the post-restart silence that masks the hardware
//! PLL relock. Platform-independent; the envelope math is unit-tested on any host.
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
/// derivative is zero at both ends: the ramp adds no click of its own.
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
/// buffer ahead); cover the ramp's buffer periods plus 3 of slack, clamped to
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
#[path = "../../tests/unit/player/declick.rs"]
mod tests;
