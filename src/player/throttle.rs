//! Decode-thread back-pressure shared by the exclusive-WASAPI and ASIO decode
//! loops (the shared-mode path blocks on its bounded ring instead).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::mpsc;
use std::time::Duration;

/// Decode-ahead target, in seconds of audio. Each backend scales it into its
/// own unit (ASIO counts interleaved samples, exclusive counts bytes).
pub(crate) const DECODE_AHEAD_SECS: u64 = 2;

/// Park the decode thread (5ms poll) while more than `throttle_hi` of audio
/// is in flight: `sent` is the decoder's own total, `consumed` the monotone
/// counter the consumer credits as audio leaves its buffers (played or
/// discarded). The delta stays honest while the consumer is stuck opening
/// the device: a cold-started decoder parks after one target's worth
/// instead of decoding the whole track into the command channel. Escapes:
/// cancel, drain, or a seek on `seek_rx` (seeded into `pending_initial_seek`,
/// clearing `was_initial_seek`); a channel-arrived pending seek skips the
/// wait (it must reach the buffer-flushing seek handler), a retried failed
/// initial seek does not (it re-arms every iteration). Returns true on cancel.
pub(crate) fn throttle_decode_ahead(
    sent: u64,
    consumed: &AtomicU64,
    throttle_hi: u64,
    cancel: &AtomicBool,
    seek_rx: &mpsc::Receiver<(f64, u32)>,
    pending_initial_seek: &mut Option<(f64, u32)>,
    was_initial_seek: &mut bool,
) -> bool {
    if (pending_initial_seek.is_none() || *was_initial_seek)
        && sent.saturating_sub(consumed.load(Relaxed)) > throttle_hi
    {
        while !cancel.load(Relaxed) && sent.saturating_sub(consumed.load(Relaxed)) > throttle_hi {
            if let Ok(t) = seek_rx.try_recv() {
                *pending_initial_seek = Some(t);
                *was_initial_seek = false;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    cancel.load(Relaxed)
}

#[cfg(test)]
#[path = "../../tests/unit/player/throttle.rs"]
mod tests;
