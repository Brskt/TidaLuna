//! Tests for `src/connect/ipc/helpers.rs`, attached to it by `#[path]`.

use super::{Queued, ServerInfoClaim, TidalConnectController, await_delivery, build_emit_js};
use crate::connect::testing::{MockWsServer, live_controller};
use crate::connect::types::MdnsDevice;
use std::sync::{Arc, Barrier, Mutex};
use std::time::Duration;

/// How long a test waits for an answer, which the mock never gives.
const SHORT: Duration = Duration::from_millis(50);

fn device() -> MdnsDevice {
    crate::connect::testing::device("d")
}

fn deliver(ctrl: &Mutex<TidalConnectController>, token: &str) -> ServerInfoClaim {
    super::deliver(
        ctrl,
        token,
        None,
        serde_json::json!({ "command": "updateServerInfo", "token": token }),
    )
}

fn queued(claim: ServerInfoClaim) -> Queued {
    match claim {
        ServerInfoClaim::Queued(queued) => queued,
        _ => panic!("owed"),
    }
}

/// A token the device never got must not stay recorded as handed over: the renderer's next
/// `setAuthentication` is the only thing that would send it again, and it would be skipped.
#[tokio::test]
async fn a_delivery_the_device_never_answers_is_given_back() {
    let mut server = MockWsServer::new().await.unwrap();
    let (ctrl, _, _conn) = live_controller(&mut server, &device()).await;

    let sent = await_delivery(&ctrl, queued(deliver(&ctrl, "a")), SHORT).await;
    assert!(sent.is_err(), "the device never answers");
    assert!(matches!(deliver(&ctrl, "a"), ServerInfoClaim::Queued(_)));
}

/// A frame the socket's write queue refused never left: the token must not stay recorded as
/// handed over, or the dedup would skip the next `setAuthentication` carrying it.
#[tokio::test]
async fn a_delivery_the_write_queue_refused_is_given_back() {
    let mut server = MockWsServer::new().await.unwrap();
    // The device never reads: the queue fills once the socket's buffers do.
    let (ctrl, _, _conn) = live_controller(&mut server, &device()).await;
    let padding = "x".repeat(1 << 20);
    let refused = (0..10_000)
        .map(|i| format!("t{i}"))
        .find(|token| {
            let command = serde_json::json!({
                "command": "updateServerInfo", "token": token, "padding": padding
            });
            matches!(
                super::deliver(&ctrl, token, None, command),
                ServerInfoClaim::Failed(_)
            )
        })
        .expect("the write queue fills up");
    assert!(!matches!(deliver(&ctrl, &refused), ServerInfoClaim::Held));
}

/// Two updates recorded in an order reach the device in that order, or it is left holding the one
/// the record says was replaced. Recording and queueing share one hold of the controller.
#[tokio::test(flavor = "multi_thread")]
async fn deliveries_reach_the_wire_in_the_order_they_were_recorded() {
    const THREADS: usize = 12;
    const EACH: usize = 5;
    let mut server = MockWsServer::new().await.unwrap();
    let (ctrl, _, mut conn) = live_controller(&mut server, &device()).await;

    let start = Arc::new(Barrier::new(THREADS));
    let workers: Vec<_> = (0..THREADS)
        .map(|t| {
            let (ctrl, start) = (ctrl.clone(), start.clone());
            std::thread::spawn(move || {
                start.wait();
                (0..EACH)
                    .map(|i| {
                        let token = format!("{t}-{i}");
                        let seq = queued(deliver(&ctrl, &token)).delivery.seq();
                        (token, seq)
                    })
                    .collect::<Vec<_>>()
            })
        })
        .collect();
    let recorded: std::collections::HashMap<String, u64> = workers
        .into_iter()
        .flat_map(|w| w.join().unwrap())
        .collect();

    let mut last = 0;
    for _ in 0..THREADS * EACH {
        let frame = conn.recv().await.expect("every delivery is sent");
        let seq = recorded[frame["token"].as_str().unwrap()];
        assert!(
            seq > last,
            "a delivery recorded earlier arrived after a later one"
        );
        last = seq;
    }
}

#[test]
fn build_emit_js_keeps_channel_in_one_string_literal() {
    // A backslash before a quote defeats a single-quote-only escape; the
    // channel must stay one inert JS string literal that round-trips.
    let evil = "x\\';alert(1)//";
    let js = build_emit_js(evil, None);
    let inner = js
        .strip_prefix("if(typeof window.__LUNAR_IPC_EMIT__==='function')window.__LUNAR_IPC_EMIT__(")
        .and_then(|s| s.strip_suffix(");"))
        .expect("emit structure intact");
    let decoded: String =
        serde_json::from_str(inner).expect("channel is a valid JSON string literal");
    assert_eq!(decoded, evil);
}
