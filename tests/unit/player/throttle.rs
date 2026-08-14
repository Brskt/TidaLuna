//! Tests for `src/player/throttle.rs`, attached to it by `#[path]`.

use super::*;

#[test]
fn skips_wait_when_below_target_or_channel_seek_pending() {
    let cancel = AtomicBool::new(false);
    let (_tx, rx) = mpsc::channel::<(f64, u32)>();

    // In-flight below target: no wait, not cancelled.
    assert!(!throttle_decode_ahead(
        5,
        &AtomicU64::new(0),
        10,
        &cancel,
        &rx,
        &mut None,
        &mut false
    ));
    // Consumer caught up: large sent total, but the delta is small.
    assert!(!throttle_decode_ahead(
        100,
        &AtomicU64::new(95),
        10,
        &cancel,
        &rx,
        &mut None,
        &mut false
    ));
    // Over target but a channel-arrived seek is pending (EOF-park case):
    // skipped for the caller's seek handler to run immediately.
    let mut pending = Some((3.0, 1));
    let mut was_initial = false;
    assert!(!throttle_decode_ahead(
        100,
        &AtomicU64::new(0),
        10,
        &cancel,
        &rx,
        &mut pending,
        &mut was_initial
    ));
    assert_eq!(pending, Some((3.0, 1)));
}

#[test]
fn cancelled_while_over_target_returns_true() {
    let cancel = AtomicBool::new(true);
    let (_tx, rx) = mpsc::channel::<(f64, u32)>();
    assert!(throttle_decode_ahead(
        100,
        &AtomicU64::new(0),
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
    let (tx, rx) = mpsc::channel::<(f64, u32)>();
    tx.send((42.0, 1)).unwrap();
    let mut pending = None;
    let mut was_initial = false;
    assert!(!throttle_decode_ahead(
        100,
        &AtomicU64::new(0),
        10,
        &cancel,
        &rx,
        &mut pending,
        &mut was_initial
    ));
    assert_eq!(pending, Some((42.0, 1)));
    assert!(!was_initial);
}

#[test]
fn failed_initial_seek_retry_stays_throttled() {
    // A retried initial seek must NOT bypass the wait. The queued live seek
    // proves the wait was entered (caught: pending overwritten, flag
    // cleared) and keeps the test sleep-free.
    let cancel = AtomicBool::new(false);
    let (tx, rx) = mpsc::channel::<(f64, u32)>();
    tx.send((42.0, 1)).unwrap();
    let mut pending = Some((5.0, 1));
    let mut was_initial = true;
    assert!(!throttle_decode_ahead(
        100,
        &AtomicU64::new(0),
        10,
        &cancel,
        &rx,
        &mut pending,
        &mut was_initial
    ));
    assert_eq!(pending, Some((42.0, 1)));
    assert!(!was_initial);
}
