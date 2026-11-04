//! What ends a websocket connection its owner closes, the receiver's and the controller's alike.

use std::time::Duration;

use futures_util::StreamExt;
use tokio_tungstenite::tungstenite::Message as WsMessage;

/// How long a close waits for what is queued to be written and for the peer's Close: past it the
/// socket is cut, a peer that stopped reading or answering holding nothing longer.
pub(crate) const CLOSE_DEADLINE: Duration = Duration::from_secs(2);

/// Reads and drops what the peer still sends, to the end of the stream: a read writes the reply a
/// peer's Close is owed, whichever Close came first. Released sooner, a socket holding unread
/// frames would be reset, and the reset discards what the peer has yet to receive.
pub(crate) async fn read_to_end<S, E>(read: &mut S)
where
    S: futures_util::Stream<Item = Result<WsMessage, E>> + Unpin,
{
    while let Some(Ok(_)) = read.next().await {}
}

/// Why a read loop stopped, which says what the peer is still owed.
pub(crate) enum ReadEnd {
    /// The peer sent its Close. tungstenite queued the reply on reading it, and only a further
    /// read, flush or close writes it: a write is refused once the peer closed.
    PeerClosed,
    /// Our token stopped it, or the stream ended or failed: no handshake is left to answer.
    Stopped,
}

/// Writes the reply a peer's Close is owed, by reading on as tungstenite's contract asks: the
/// first read flushes it. On the receiver's side the stream ends there; the controller's waits
/// for the device to drop TCP, as RFC 6455 has a server do first. Bounded by the deadline.
pub(crate) async fn answer_close<S, E>(end: ReadEnd, read: &mut S)
where
    S: futures_util::Stream<Item = Result<WsMessage, E>> + Unpin,
{
    if let ReadEnd::PeerClosed = end {
        let _ = tokio::time::timeout(CLOSE_DEADLINE, read_to_end(read)).await;
    }
}
