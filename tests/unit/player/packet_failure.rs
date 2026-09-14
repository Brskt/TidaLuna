//! Tests for `src/player/packet_failure.rs`, attached to it by `#[path]`.

use super::*;
use std::io::ErrorKind;
use symphonia::core::errors::Error;

fn io(kind: ErrorKind) -> Error {
    Error::IoError(std::io::Error::new(kind, "whatever the sink said"))
}

/// The two ways a dead connection reaches a packet read, and the reason both have to count.
/// The reader gives up after thirty seconds of nothing (`TimedOut`); the writer gives up
/// first and stores its own failure, which the buffer hands back as `ConnectionAborted`. On
/// a pulled cable the writer wins that race, so listening for the timeout alone would let a
/// cut network keep arriving as a file that will not read.
#[test]
fn a_stalled_connection_is_a_network_failure() {
    for kind in [ErrorKind::TimedOut, ErrorKind::ConnectionAborted] {
        assert_eq!(
            classify_packet_failure(&io(kind)),
            PacketFailure::Network,
            "{kind:?} is how a dead connection reaches a decoder"
        );
    }
}

/// The bytes arrived and will not decode. Saying so is what lets the queue move on, which is
/// exactly what a stall must not do.
#[test]
fn a_malformed_stream_is_a_source_failure() {
    assert_eq!(
        classify_packet_failure(&Error::DecodeError("malformed frame")),
        PacketFailure::Source,
        "a stream that will not decode is not a connection problem"
    );
}

/// An IO error that is not a stall: a genuine fault on bytes that did arrive. Blaming the
/// connection here would hold a queue that has nothing left to wait for.
#[test]
fn an_unrelated_io_error_is_a_source_failure() {
    assert_eq!(
        classify_packet_failure(&io(ErrorKind::InvalidData)),
        PacketFailure::Source,
        "only a stall names the connection; every other io error names the data"
    );
}
