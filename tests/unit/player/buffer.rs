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
        std::io::ErrorKind::Other,
        "never Interrupted: symphonia's read_buf_exact retries that kind forever"
    );
}

/// Three ways a read can fail, three answers the decoder owes, three kinds.
///
/// A dead network raises the no-connection banner and holds the queue. A deliberate stop says
/// nothing, someone having asked for it. An unusable source reports a media error and lets the
/// queue advance, the opposite of the network's answer, and the reason the two cannot share a
/// kind.
///
/// The writer's retry budget (eight reconnects, about fourteen seconds) runs out well before
/// the reader's thirty, so on a pulled cable it is ALWAYS `finish_with_error` that speaks,
/// never the read timeout. Listening only for the timeout, a cut network reached the listener
/// as "unexpected error (NPO03)". The correction swept the other way: `finish_with_error`
/// gives up for six reasons and only two are the network, so an expired url's 403 and a key
/// that will not decrypt both announced "no internet" over a healthy connection.
#[test]
fn the_three_ways_a_read_fails_report_three_kinds() {
    let mut sink = [0u8; 16];

    let (failed, writer) = RamBuffer::new_for_test(1024);
    writer.finish_with_error(
        DownloadFailure::Network,
        "network error after 8 reconnects".to_string(),
    );
    let network = failed
        .clone()
        .read(&mut sink)
        .expect_err("a failed download cannot read")
        .kind();

    let (unreadable, writer3) = RamBuffer::new_for_test(1024);
    writer3.finish_with_error(
        DownloadFailure::Source,
        "range request status: 403".to_string(),
    );
    let source = unreadable
        .clone()
        .read(&mut sink)
        .expect_err("a failed download cannot read")
        .kind();

    let (cancelled, _writer2) = RamBuffer::new_for_test(1024);
    cancelled.cancel();
    let stopped = cancelled
        .clone()
        .read(&mut sink)
        .expect_err("a cancelled buffer cannot read")
        .kind();

    assert_ne!(
        network, stopped,
        "the decoder cannot tell a dead network from a stop it was asked for"
    );
    assert_ne!(
        network, source,
        "a rejected status announces no internet over a healthy connection"
    );
    assert_ne!(
        source, stopped,
        "an unusable source has to be announced, unlike a stop"
    );
    assert_eq!(network, std::io::ErrorKind::ConnectionAborted);
    assert_eq!(source, std::io::ErrorKind::InvalidData);
    for kind in [network, source, stopped] {
        assert_ne!(
            kind,
            std::io::ErrorKind::Interrupted,
            "never Interrupted: symphonia's read_buf_exact retries that kind forever"
        );
    }
}

/// The kind keeps symphonia out of an unbounded retry; the named `ReadStop` in the payload is
/// what the decoder reads back. Symphonia's `read_buf_exact` retries
/// `Interrupted` without limit, and both stop flags are latched; a read reporting that
/// kind returns instantly on every retry: a decode thread spinning at full CPU inside
/// `next_packet`, and `cancel_crossfade` blocked on its join for good. Unreachable while
/// the incoming buffer is always complete; reachable the moment it streams.
#[test]
fn a_stopped_read_never_reports_a_kind_symphonia_retries() {
    let (cancelled, _writer) = RamBuffer::new_for_test(1024);
    cancelled.cancel();
    let mut sink = [0u8; 16];
    assert_ne!(
        cancelled
            .clone()
            .read(&mut sink)
            .expect_err("a cancelled buffer cannot read")
            .kind(),
        std::io::ErrorKind::Interrupted
    );

    let (buffer, _writer2) = RamBuffer::new_for_test(1024);
    let retired = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    assert_ne!(
        buffer
            .with_reader_cancel(retired)
            .read(&mut sink)
            .expect_err("a retired reader cannot read")
            .kind(),
        std::io::ErrorKind::Interrupted
    );
}

/// Which stop it was has to survive the trip through `io::Error`, because the kind cannot say:
/// `Other` is shared with symphonia, which mints its own inside the Vorbis bit reader. Read back
/// off the payload, the site that knew why is the site that says so, and a real failure must
/// not read back as a stop, since the answers the two owe the queue are opposite.
#[test]
fn a_stop_carries_which_stop_it_was() {
    let mut sink = [0u8; 16];

    let (cancelled, _writer) = RamBuffer::new_for_test(1024);
    cancelled.cancel();
    let err = cancelled
        .clone()
        .read(&mut sink)
        .expect_err("a cancelled buffer cannot read");
    assert_eq!(ReadStop::from_io(&err), Some(ReadStop::StreamCancelled));

    let (buffer, _writer2) = RamBuffer::new_for_test(1024);
    let retired = Arc::new(AtomicBool::new(true));
    let err = buffer
        .with_reader_cancel(retired)
        .read(&mut sink)
        .expect_err("a retired reader cannot read");
    assert_eq!(ReadStop::from_io(&err), Some(ReadStop::ReaderRetired));

    let (failed, writer3) = RamBuffer::new_for_test(1024);
    writer3.finish_with_error(
        DownloadFailure::Network,
        "network error after 8 reconnects".to_string(),
    );
    let err = failed
        .clone()
        .read(&mut sink)
        .expect_err("a failed download cannot read");
    assert_eq!(
        ReadStop::from_io(&err),
        None,
        "a dead network is not a stop anyone asked for"
    );
}

/// A staged track is published while it is still filling, and becoming current is a fact
/// about the BUFFER. The download's two decisions (which bucket pays, which url a
/// reconnect uses) are read from it per chunk, and the handover reaches a task that was
/// spawned long before it. Held by value in that task, as it was, it could not change at
/// all: a hi-res track adopted from a preload stayed pinned to the fixed preload rate,
/// which sits below what it needs to play in real time.
#[tokio::test]
async fn adoption_moves_a_staged_download_to_playback() {
    let (buffer, writer) = RamBuffer::new(
        1024,
        DownloadOwner::Preload,
        tokio_util::sync::CancellationToken::new(),
    );
    assert_eq!(
        writer.owner(),
        DownloadOwner::Preload,
        "it starts out staged ahead of the listener"
    );

    // What `commit_peeked` and `take_preloaded_if_match` do when the staged record is spent.
    buffer.adopt_as_playback();

    assert_eq!(
        writer.owner(),
        DownloadOwner::Playback,
        "the task filling it is told without being restarted"
    );
}

/// The token lives in the buffer so that handing the buffer over hands the download over
/// with it. Both adoption sites used to clear the only slot naming it, after which nothing
/// could stop that download at all.
#[tokio::test]
async fn the_buffer_carries_the_token_that_stops_its_download() {
    let token = tokio_util::sync::CancellationToken::new();
    let (buffer, writer) = RamBuffer::new(1024, DownloadOwner::Preload, token.clone());

    assert!(!writer.cancel_token().is_cancelled());
    buffer.cancel_download();

    assert!(
        writer.cancel_token().is_cancelled(),
        "a holder of the buffer alone can stop the task filling it"
    );
    assert!(
        token.is_cancelled(),
        "and the caller's own copy names the same download"
    );
}

/// A complete buffer has no task filling it. Its handle answers as already stopped rather
/// than as missing, which is what lets every caller treat the two the same.
#[tokio::test]
async fn a_complete_buffer_has_nothing_left_to_stop() {
    let buffer = RamBuffer::from_complete(vec![0u8; 16]);
    buffer.cancel_download();
    assert!(
        buffer.is_same_stream(&buffer.clone()),
        "a clone is the same stream"
    );
}

/// `set_current_buffer` stops the download of the buffer it displaces, and the device
/// rebuild and the exclusive and ASIO respawns reinstall a CLONE of the one already
/// current. Telling those apart is what keeps a rebuild from killing the download of the
/// track still playing.
#[tokio::test]
async fn a_clone_is_not_a_different_stream() {
    let (first, _writer) = RamBuffer::new_for_test(1024);
    let (second, _writer2) = RamBuffer::new_for_test(1024);

    assert!(first.is_same_stream(&first.clone()));
    assert!(
        !first.is_same_stream(&second),
        "same size, different download"
    );
}

/// A body that ends under its announced length is NOT a complete file, and everything that
/// keeps these bytes has to be told so.
///
/// `finish()` says the stream ended, not that the file arrived: an HTTP/2 `RST_STREAM` with
/// `NO_ERROR` mid-body, and a reconnect answered `416`, both end a short transfer with no
/// error set at all. Reading `finished` alone, the three consumers here each keep a truncated
/// track: two hand it to the disk cache, where it is indexed as whole and served as valid on
/// every later play, and the third replays it from RAM after a device switch.
#[tokio::test]
async fn a_body_that_ends_under_its_announced_length_is_not_a_whole_file() {
    let (short, writer) = RamBuffer::new_for_test(4096);
    writer.write_counted(&[1u8; 1024]);
    writer.set_ciphertext(
        tempfile::NamedTempFile::new().expect("a temp file for the staged ciphertext"),
        1024,
    );
    writer.finish();

    assert!(
        !short.is_complete(),
        "1 KB of an announced 4 KB is not the entire file, whatever `finished` says"
    );
    assert!(
        short.take_ciphertext().is_none(),
        "a truncated ciphertext handed over is a truncated track indexed as whole"
    );
    assert!(
        !short.is_reusable(),
        "reusing it replays the truncation instead of rebuilding from the source"
    );

    // The whole body, same announced length: the answer every healthy download gets, and the
    // one a length comparison must not cost.
    let (whole, writer) = RamBuffer::new_for_test(4096);
    writer.write_counted(&[1u8; 4096]);
    writer.set_ciphertext(
        tempfile::NamedTempFile::new().expect("a temp file for the staged ciphertext"),
        4096,
    );
    writer.finish();

    assert!(whole.is_complete(), "the announced length arrived in full");
    assert!(
        whole.take_ciphertext().is_some(),
        "a complete download is what the disk cache exists to keep"
    );
    assert!(
        whole.is_reusable(),
        "a complete buffer still in memory is what a device switch reuses"
    );
}
