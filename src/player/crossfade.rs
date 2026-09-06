//! Equal-power crossfade envelopes and the sample-summing step, shared by the
//! cpal callback. The envelope math is platform-independent and unit-tested on any
//! host; the one exception is `crossfade_accepts_rate`, whose `pinned` argument
//! answers a platform-specific question and is exercised for both answers by its test.
//!
//! Distinct from `declick.rs` on purpose. Those envelopes sum to one, which is
//! right for de-click, where both sides are the same signal. Two different tracks
//! summed that way lose 3 dB of power at the midpoint; this pair squares to one
//! instead.

/// Longest crossfade the UI offers, in seconds. Matches TIDAL's own slider.
pub(crate) const MAX_CROSSFADE_SECS: u8 = 12;

/// Descending quarter-cosine coefficient for the track being faded out.
#[inline]
pub(crate) fn xfade_out_env(pos: usize, len: usize) -> f32 {
    let t = pos as f32 / len.max(1) as f32;
    (t * std::f32::consts::FRAC_PI_2).cos()
}

/// Rising quarter-sine coefficient for the track being faded in. Paired with
/// `xfade_out_env`, the squares sum to one at every position.
#[inline]
pub(crate) fn xfade_in_env(pos: usize, len: usize) -> f32 {
    let t = pos as f32 / len.max(1) as f32;
    (t * std::f32::consts::FRAC_PI_2).sin()
}

/// Shorten a fade to the audio the incoming track has actually staged.
///
/// The staged buffer is published while it is still filling: what has landed by
/// arming time is the real limit rather than the track's size. Bytes per second is taken
/// from the track's own size and duration, which holds for CD and hi-res alike; an
/// unknown duration or size leaves the fade untouched, because guessing short here costs
/// a fade that would have worked. Pure, to be unit-tested without the audio pipeline.
pub(crate) fn fade_len_from_staged(
    len_samples: usize,
    staged_bytes: u64,
    total_bytes: u64,
    duration: f64,
    per_second: u64,
) -> usize {
    // NaN and the infinities are ruled out explicitly: they slip past a plain `<= 0.0`,
    // and a NaN ratio casts to zero samples, which would silently refuse every fade.
    if !duration.is_finite() || duration <= 0.0 || total_bytes == 0 {
        return len_samples;
    }
    let bytes_per_second = total_bytes as f64 / duration;
    if !bytes_per_second.is_finite() || bytes_per_second <= 0.0 {
        return len_samples;
    }
    let staged_samples = (staged_bytes as f64 / bytes_per_second * per_second as f64) as usize;
    len_samples.min(staged_samples)
}

/// Re-fit a running fade whose incoming track has no more than `available` samples left
/// to give, ending the envelope on audio that exists instead of on silence.
///
/// Returns the new `(pos, len)`, or `None` when the fade already fits. Both move
/// together on purpose: changing `len` alone jumps the phase from `pos/len` to
/// `pos/len'`, and a gain step is a click. With `M` samples left at position `N`,
/// `len' = M*len/(len-N)` and `pos' = len'-M` give `pos'/len' = N/len` (the phase is
/// unchanged at the splice) and `(pos'+M)/len' = 1`; unity lands on the last sample
/// that actually exists. Pure, to be unit-tested without the audio pipeline.
pub(crate) fn refit_fade(pos: usize, len: usize, available: usize) -> Option<(usize, usize)> {
    let remaining = len.saturating_sub(pos);
    if available >= remaining {
        return None;
    }
    // Nothing left at all is NOT this function's case. Jumping the position to `len`
    // drops the OUTGOING envelope from cos(N/len) to zero in one callback (the very
    // gain step this exists to avoid), and it does not bring the swap forward either,
    // since that also waits on the outgoing track's own end. The result was silence on
    // both sides for the rest of the outgoing track. An incoming track with nothing
    // left is the "shorter than the fade" case, which the mixer already degrades to a
    // plain fade-out by contributing exact zeros.
    if available == 0 {
        return None;
    }
    let scaled = (available as u64 * len as u64) / remaining.max(1) as u64;
    let new_len = (scaled as usize).max(available);
    Some((new_len - available, new_len))
}

/// Sum one callback's worth of both sources into `dst`, applying the envelopes and
/// the master volume. `pos` is the fade position in samples at entry; the returned
/// position saturates at `len`.
///
/// A short or empty `incoming` contributes exact zeros for the samples it cannot
/// supply. A decoder that has not buffered yet degrades the fade to a plain
/// fade-out rather than glitching.
pub(crate) fn mix_frames(
    dst: &mut [f32],
    outgoing: &[f32],
    incoming: &[f32],
    pos: usize,
    len: usize,
    volume: f32,
) -> usize {
    for (i, d) in dst.iter_mut().enumerate() {
        let p = (pos + i).min(len);
        let out = outgoing.get(i).copied().unwrap_or(0.0) * xfade_out_env(p, len);
        let inc = incoming.get(i).copied().unwrap_or(0.0) * xfade_in_env(p, len);
        *d = (out + inc) * volume;
    }
    (pos + dst.len()).min(len)
}

/// Whether the decoder has reached the point where a fade of `secs` should start.
///
/// Every uncertain input fails closed. An unknown total length, a zero rate, or a
/// track no longer than the fade itself all mean no crossfade: arming on a guess
/// would cut a track short.
pub(crate) fn crossfade_should_arm(
    produced: u64,
    total: u64,
    rate: u32,
    channels: u16,
    secs: u8,
) -> bool {
    if secs == 0 || total == 0 || rate == 0 || channels == 0 {
        return false;
    }
    let fade = u64::from(rate) * u64::from(channels) * u64::from(secs);
    // A track no longer than the fade cannot host one.
    if total <= fade {
        return false;
    }
    produced >= total - fade
}

/// How long a fade may actually run, given how much of the outgoing track is
/// still unplayed. `None` refuses the fade outright.
///
/// The arming predicate is sticky: once a track is inside its window it stays
/// there until the end. A next track that only becomes available late must not
/// arm the full configured length: the outgoing ring would empty long before the
/// fade reached it, `played_samples` would freeze, and the ordinary drain check
/// would read that as a finished track and tear the stream down mid-fade.
/// Clamping to what remains is what keeps the swap reachable.
///
/// Below a second there is no fade worth hearing and the hard cut is cleaner.
pub(crate) fn fade_len_samples(per_second: u64, secs: u8, remaining: u64) -> Option<usize> {
    if per_second == 0 || secs == 0 {
        return None;
    }
    let len = (per_second * u64::from(secs)).min(remaining);
    (len >= per_second).then_some(len as usize)
}

/// Whether a fade may arm when the incoming track's native rate differs from the
/// stream's. `pinned` is whether the engine's output rate is pinned to the device
/// rather than reopened per track; the caller reads that off `ENGINE_RATE_IS_PINNED`
/// keeping this a pure function of its inputs, testable for both answers on
/// any host.
///
/// The incoming decoder conforms to the stream either way: the fade itself is never
/// the problem. What differs is the aftermath: where the stream is reopened per track,
/// the promoted track would keep playing at the OUTGOING track's rate until the next
/// load, and the OS would convert on top of that: two conversions, the first of which
/// may have thrown away everything above its own Nyquist. Where the engine rate is
/// pinned to the device, there is no aftermath: the promoted track is already at the
/// rate the device wants.
pub(crate) fn crossfade_accepts_rate(incoming: u32, stream: u32, pinned: bool) -> bool {
    incoming == stream || pinned
}

#[cfg(test)]
#[path = "../../tests/unit/player/crossfade.rs"]
mod tests;
