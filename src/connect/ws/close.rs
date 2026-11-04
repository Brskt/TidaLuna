//! What ends a websocket connection its owner closes, the receiver's and the controller's alike.

use std::time::Duration;

use futures_util::StreamExt;
use tokio_tungstenite::tungstenite::Message as WsMessage;

/// How long a close waits for what is queued to be written and for the peer's Close: past it the
/// socket is cut, a peer that stopped reading or answering holding nothing longer.
pub(crate) const CLOSE_DEADLINE: Duration = Duration::from_secs(2);

/// Reads and drops what the peer still sends, up to its Close: the answer to ours, and the last
/// frame it sends. Released before it, a socket holding a Pong or a command not yet read would
/// be reset, and the reset discards what the peer has yet to receive, our Close included.
pub(crate) async fn read_to_close<S, E>(read: &mut S)
where
    S: futures_util::Stream<Item = Result<WsMessage, E>> + Unpin,
{
    while let Some(Ok(message)) = read.next().await {
        if let WsMessage::Close(_) = message {
            break;
        }
    }
}
