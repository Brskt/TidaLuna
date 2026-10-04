//! The writer each websocket connection runs, the controller's and every receiver client's: it
//! forwards its queue to the socket until the queue closes or the token is cancelled.

use futures_util::SinkExt;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_util::sync::CancellationToken;

/// Generic over the sink: each side's socket has its own type, and a test stands in for either.
pub(crate) async fn run<S>(
    mut write: S,
    mut rx: mpsc::Receiver<WsMessage>,
    cancel: CancellationToken,
) where
    S: futures_util::Sink<WsMessage> + Unpin,
{
    // The token is raced against every await, a send parked on a peer that stopped reading
    // included. A cancelled writer offers the close frame once and leaves without waiting on it.
    loop {
        let msg = tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            msg = rx.recv() => msg,
        };
        let Some(m) = msg else {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {}
                _ = write.close() => {}
            }
            return;
        };
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            sent = write.send(m) => if sent.is_err() {
                break;
            },
        }
    }
    let _ = futures_util::FutureExt::now_or_never(write.close());
}
