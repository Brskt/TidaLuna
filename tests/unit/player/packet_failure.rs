//! Tests for `src/player/packet_failure.rs`, attached to it by `#[path]`.

use super::*;
use std::io::ErrorKind;
use symphonia::core::errors::Error;

fn io(kind: ErrorKind) -> Error {
    Error::IoError(std::io::Error::new(kind, "whatever the sink said"))
}

/// A dead connection reaches a packet read as `TimedOut`, or as `ConnectionAborted` when the
/// writer gives up first (it wins that race on a pulled cable), and both are the network. Every
/// other error names the data: blaming the connection would hold a queue with nothing to wait for.
#[test]
fn only_a_stalled_connection_is_a_network_failure() {
    for (what, error, expected) in [
        ("a timeout", io(ErrorKind::TimedOut), PacketFailure::Network),
        (
            "an aborted connection",
            io(ErrorKind::ConnectionAborted),
            PacketFailure::Network,
        ),
        (
            "a malformed stream",
            Error::DecodeError("malformed frame"),
            PacketFailure::Source,
        ),
        (
            "an unrelated io error",
            io(ErrorKind::InvalidData),
            PacketFailure::Source,
        ),
    ] {
        assert_eq!(classify_packet_failure(&error), expected, "{what}");
    }
}
