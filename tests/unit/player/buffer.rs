//! Tests for `src/player/buffer.rs`, attached to it by `#[path]`.
//!
//! `#[tokio::test]` throughout, as in `thread/commands.rs`: the Range-restart arm site asks
//! the governor for the boosted rate, and `GOVERNOR`'s `LazyLock` init calls `tokio::spawn`.
//! A plain `#[test]` that reaches it panics for want of a reactor and leaves the static
//! poisoned for every later test in the binary: the blast radius of getting this wrong is
//! the whole suite, not this file.

use super::*;
use std::io::{Read, Seek};

/// A reader starved at the download frontier parks inside `read`, where it answers no command.
/// Whoever joins that thread waits with it; retiring the reader has to end the wait on the
/// signal rather than on the read's own 5s timeout, which is the margin measured here.
#[tokio::test]
async fn a_retired_reader_stops_waiting_on_the_signal_not_the_timeout() {
    // Held for the test: dropping the writer cancels the whole buffer, which would end the
    // wait for a different reason than the one under test.
    let (buffer, _writer) = RamBuffer::new_for_test(1024);
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
    assert_eq!(
        err.kind(),
        std::io::ErrorKind::Other,
        "never Interrupted: symphonia's read_buf_exact retries that kind forever"
    );
    assert!(
        waited < std::time::Duration::from_secs(2),
        "woke on its own timeout rather than on the signal: waited {waited:?}"
    );
}

/// `cancel()` has no undo, and every read after it fails. A finished buffer that
/// was cancelled reads no better than an empty one: the case this predicate promises
/// its callers it has ruled out, and the one a failed load leaves sitting in `current_buffer`.
#[tokio::test]
async fn a_cancelled_buffer_is_not_reusable_even_once_complete() {
    let (buffer, writer) = RamBuffer::new_for_test(4);
    let _ = writer.write_counted(b"data");
    writer.finish();
    assert!(
        buffer.is_reusable(),
        "a complete buffer with its bytes present is the reusable case"
    );

    buffer.cancel();

    assert!(
        !buffer.is_reusable(),
        "a decoder handed this buffer would fail on its first read"
    );
}

/// Retirement is per-reader, which is what a device switch relies on when it respawns a
/// decoder on the same bytes. Cancelling the buffer instead would take those bytes with it.
#[tokio::test]
async fn retiring_one_reader_leaves_the_buffer_readable_by_the_next() {
    let (buffer, writer) = RamBuffer::new_for_test(4);
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

/// The arm site itself, the one that now asks the governor to hurry. A cursor further ahead
/// than the buffer is willing to wait through has to turn into a restart request, and reaching
/// that decision must not blow up on the runtime-less thread a decoder actually reads from.
/// Forcing `GOVERNOR` from here exercises exactly that.
#[tokio::test]
async fn a_read_far_past_the_frontier_arms_a_range_restart() {
    // Force the governor's init from inside this test's runtime. The reader below is a raw
    // `std::thread`, which does NOT inherit one (just like the decode thread in production,
    // where `main.rs` does exactly this at startup). Without it the arm site's first touch
    // lands on a runtime-less thread, panics, and poisons the static for the whole binary.
    let _ = &*crate::state::GOVERNOR;

    let (buffer, writer) = RamBuffer::new_for_test(1024 * 1024);
    let _ = writer.write_counted(&[0u8; 1024]);
    let mut reader = buffer.clone();
    // Past the written frontier by more than the lookahead the buffer waits through: the
    // read asks for a restart instead of sitting on the download catching up.
    reader
        .seek(std::io::SeekFrom::Start(1024 + 64 * 1024))
        .expect("seeking a RamBuffer moves its cursor and nothing else");

    let reading = std::thread::spawn(move || {
        let mut sink = [0u8; 64];
        reader.read(&mut sink)
    });
    // The request is posted before the wait begins.
    std::thread::sleep(std::time::Duration::from_millis(50));

    assert!(
        writer.has_restart_pending(),
        "a cursor past the lookahead has to move the download, not wait for it"
    );

    // Release the parked reader, then insist on how it ended: the arm site forces GOVERNOR's
    // init on this very thread, and a discarded join would hide that panic behind an assertion
    // that passes anyway, since the request is posted before the governor is ever touched.
    buffer.cancel();
    let outcome = reading
        .join()
        .expect("the reading thread must not panic: the arm site touches GOVERNOR");
    assert_eq!(
        outcome
            .expect_err("the cancel above ends the parked read")
            .kind(),
        std::io::ErrorKind::Interrupted
    );
}
