//! Tests for `src/player/buffer.rs`, attached to it by `#[path]`.
//!
//! Retirement is what these exercise. A reader starved at the download frontier parks inside
//! `read` and answers no command, so the signal that ends its wait has to reach it there.

use super::*;
use std::io::Read;

/// A reader starved at the download frontier parks inside `read`, where it answers no command.
/// Whoever joins that thread waits with it, so retiring the reader has to end the wait on the
/// signal rather than on the read's own 5s timeout, which is the margin measured here.
#[tokio::test]
async fn a_retired_reader_stops_waiting_on_the_signal_not_the_timeout() {
    // Held for the test: dropping the writer cancels the whole buffer, which would end the
    // wait for a different reason than the one under test.
    let (buffer, _writer) = RamBuffer::new(1024);
    let cancel = Arc::new(AtomicBool::new(false));
    let mut reader = buffer.clone().with_reader_cancel(cancel.clone());

    let reading = std::thread::spawn(move || {
        let mut sink = [0u8; 64];
        reader.read(&mut sink)
    });

    // Let it reach the wait first: signalling before the read starts proves nothing about
    // waking a parked reader.
    std::thread::sleep(std::time::Duration::from_millis(50));
    let signalled_at = std::time::Instant::now();
    cancel.store(true, Relaxed);
    buffer.wake_readers();
    let outcome = reading.join().expect("the reader thread must not panic");
    let waited = signalled_at.elapsed();

    let err = outcome.expect_err("a retired reader reports instead of returning bytes");
    assert_eq!(err.kind(), std::io::ErrorKind::Interrupted);
    assert!(
        waited < std::time::Duration::from_secs(2),
        "woke on its own timeout rather than on the signal: waited {waited:?}"
    );
}

/// `cancel()` has no undo, and every read after it reports Interrupted. A finished buffer that
/// was cancelled reads no better than an empty one: the case this predicate promises
/// its callers it has ruled out, and the one a failed load leaves sitting in `current_buffer`.
#[tokio::test]
async fn a_cancelled_buffer_is_not_reusable_even_once_complete() {
    let (buffer, writer) = RamBuffer::new(4);
    let _ = writer.write_counted(b"data");
    writer.finish();
    assert!(
        buffer.is_reusable(),
        "a complete buffer with its bytes present is the reusable case"
    );

    buffer.cancel();

    assert!(
        !buffer.is_reusable(),
        "a decoder handed this buffer would report Interrupted on its first read"
    );
}

/// Retirement is per-reader, which is what a device switch relies on when it respawns a
/// decoder on the same bytes. Cancelling the buffer instead would take those bytes with it.
#[tokio::test]
async fn retiring_one_reader_leaves_the_buffer_readable_by_the_next() {
    let (buffer, writer) = RamBuffer::new(4);
    let cancel = Arc::new(AtomicBool::new(false));
    let retired = buffer.clone().with_reader_cancel(cancel.clone());
    cancel.store(true, Relaxed);

    let _ = writer.write_counted(b"data");
    writer.finish();

    let mut fresh = buffer.clone();
    let mut sink = [0u8; 4];
    fresh
        .read_exact(&mut sink)
        .expect("a reader taken after the retirement reads the bytes normally");
    assert_eq!(&sink, b"data");
    assert!(
        buffer.is_reusable(),
        "a retired reader must not mark the buffer itself unusable"
    );
    drop(retired);
}
