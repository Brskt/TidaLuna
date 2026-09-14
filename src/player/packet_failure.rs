//! Why a packet read failed, shared by the three decode engines.
//!
//! Platform-independent, and unit-tested on any host: the classification is a property of
//! the error, not of the backend that received it.

/// Why a `next_packet()` call failed. Two outcomes, so a named enum: the answers the player
/// owes are opposite, and a bool at the call site would leave each engine free to name its
/// own. A dead connection stops playback where it stands and holds the queue; an unusable
/// source reports a media error and lets the queue advance. Reported as one kind they are
/// indistinguishable, which is how a pulled cable reached a listener as an unreadable file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PacketFailure {
    /// The bytes stopped arriving: the reader waited out its stall budget (`TimedOut`), or
    /// the download gave up first and stored that (`ConnectionAborted`).
    Network,
    /// The bytes arrived and cannot be decoded. The same read would fail the same way.
    Source,
}

/// Classify a packet-read failure. Only a stall names the connection; every other error,
/// including an IO error of another kind, names the data.
pub(crate) fn classify_packet_failure(e: &symphonia::core::errors::Error) -> PacketFailure {
    match e {
        symphonia::core::errors::Error::IoError(io)
            if matches!(
                io.kind(),
                std::io::ErrorKind::TimedOut | std::io::ErrorKind::ConnectionAborted
            ) =>
        {
            PacketFailure::Network
        }
        _ => PacketFailure::Source,
    }
}

#[cfg(test)]
#[path = "../../tests/unit/player/packet_failure.rs"]
mod tests;
