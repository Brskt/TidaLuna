//! Decode-thread back-pressure shared by the exclusive-WASAPI and ASIO decode
//! loops (the shared-mode path blocks on its bounded ring instead).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::mpsc;
use std::time::Duration;

/// Decode-ahead target, in seconds of audio. Each backend scales it into its
/// own buffered unit (ASIO counts interleaved samples, exclusive counts bytes).
pub(crate) const DECODE_AHEAD_SECS: u64 = 2;

/// Park the decode thread (5ms poll) while the consumer holds more than
/// `throttle_hi` of unconsumed audio: a cached source otherwise decodes the
/// whole track at once. Escapes: cancel, drain, or a seek on `seek_rx`
/// (seeded into `pending_initial_seek`, clearing `was_initial_seek` like the
/// loops' own drain). A channel-arrived pending seek skips the wait -- it must
/// reach the buffer-flushing seek handler (paused-and-full never drains); a
/// retried FAILED initial seek does not -- it re-arms every iteration, and
/// skipping would decode the whole track unthrottled. Returns true on cancel.
pub(crate) fn throttle_decode_ahead(
    buffered: &AtomicU64,
    throttle_hi: u64,
    cancel: &AtomicBool,
    seek_rx: &mpsc::Receiver<f64>,
    pending_initial_seek: &mut Option<f64>,
    was_initial_seek: &mut bool,
) -> bool {
    if (pending_initial_seek.is_none() || *was_initial_seek) && buffered.load(Relaxed) > throttle_hi
    {
        while !cancel.load(Relaxed) && buffered.load(Relaxed) > throttle_hi {
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
mod tests {
    use super::*;

    #[test]
    fn skips_wait_when_below_target_or_channel_seek_pending() {
        let cancel = AtomicBool::new(false);
        let (_tx, rx) = mpsc::channel::<f64>();

        // Below target: no wait, not cancelled.
        assert!(!throttle_decode_ahead(
            &AtomicU64::new(0),
            10,
            &cancel,
            &rx,
            &mut None,
            &mut false
        ));
        // Full but a channel-arrived seek pends (EOF-park case): wait skipped.
        let mut pending = Some(3.0);
        let mut was_initial = false;
        assert!(!throttle_decode_ahead(
            &AtomicU64::new(100),
            10,
            &cancel,
            &rx,
            &mut pending,
            &mut was_initial
        ));
        assert_eq!(pending, Some(3.0));
    }

    #[test]
    fn cancelled_while_full_returns_true() {
        let cancel = AtomicBool::new(true);
        let (_tx, rx) = mpsc::channel::<f64>();
        assert!(throttle_decode_ahead(
            &AtomicU64::new(100),
            10,
            &cancel,
            &rx,
            &mut None,
            &mut false
        ));
    }

    #[test]
    fn seek_during_wait_breaks_out_and_seeds_pending() {
        let cancel = AtomicBool::new(false);
        let (tx, rx) = mpsc::channel::<f64>();
        tx.send(42.0).unwrap();
        let mut pending = None;
        let mut was_initial = false;
        assert!(!throttle_decode_ahead(
            &AtomicU64::new(100),
            10,
            &cancel,
            &rx,
            &mut pending,
            &mut was_initial
        ));
        assert_eq!(pending, Some(42.0));
        assert!(!was_initial);
    }

    #[test]
    fn failed_initial_seek_retry_stays_throttled() {
        // A retried initial seek must NOT bypass the wait. The queued live seek
        // proves the wait was entered (caught: pending overwritten, flag
        // cleared) and keeps the test sleep-free.
        let cancel = AtomicBool::new(false);
        let (tx, rx) = mpsc::channel::<f64>();
        tx.send(42.0).unwrap();
        let mut pending = Some(5.0);
        let mut was_initial = true;
        assert!(!throttle_decode_ahead(
            &AtomicU64::new(100),
            10,
            &cancel,
            &rx,
            &mut pending,
            &mut was_initial
        ));
        assert_eq!(pending, Some(42.0));
        assert!(!was_initial);
    }
}
