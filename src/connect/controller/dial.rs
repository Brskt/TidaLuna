//! The device a connect request names, resolved by mDNS and dialled at the address Rust found.
//!
//! The renderer, plugins included, supplies only the key (`fullname`): the address and port
//! dialled come from the discovered list, never from the request.

use std::sync::Mutex;
use std::time::Duration;

use tokio::sync::mpsc;

use crate::connect::controller::TidalConnectController;
use crate::connect::types::MdnsDevice;
use crate::connect::ws::client::{WsClient, WsClientEvent};
use crate::util::truncate_str;

/// How long a connect request waits for mDNS to resolve the device it names. A browse sends its
/// PTR query after a 10-50 ms jitter, then at 1 s, 3 s and 7 s (mdns-sd 0.21.6 service_daemon.rs
/// l.138-139, l.4120-4140); 5 s spans three of them plus the 0.2-1.6 s resolve retries (l.94-98).
pub(crate) const RESOLVE_DEADLINE: Duration = Duration::from_secs(5);

/// Longest part of renderer-supplied text (a `fullname`, the echo of a malformed device) that
/// reaches a log line or an answer.
const FULLNAME_LOG_MAX: usize = 128;

/// Why a connect request did not end in a connected device. Each variant names its own answer
/// to the renderer; the caller reports all of them the same way.
#[derive(Clone)]
pub(crate) enum ConnectFailure {
    NoRuntime,
    NoController,
    Malformed(String),
    Unresolved(String),
    NoAddress,
    Dial(String),
    /// A request for another device replaced this one while it was in flight.
    Superseded,
    /// The request running the attempt ended without an answer (a panic, a cancelled task).
    Aborted,
}

impl ConnectFailure {
    pub(crate) fn status(&self) -> i32 {
        match self {
            Self::Malformed(_) => 400,
            Self::Unresolved(_) => 404,
            Self::Superseded => 409,
            Self::NoRuntime
            | Self::NoController
            | Self::NoAddress
            | Self::Dial(_)
            | Self::Aborted => 500,
        }
    }

    pub(crate) fn message(&self) -> String {
        match self {
            Self::NoRuntime => "No runtime".to_string(),
            Self::NoController => "No controller".to_string(),
            Self::Malformed(e) => format!(
                "Malformed device: {:?}",
                truncate_str(e, FULLNAME_LOG_MAX)
            ),
            Self::Aborted => "The connect attempt ended without an answer".to_string(),
            Self::Unresolved(fullname) => format!(
                "Device {:?} not found on the network",
                truncate_str(fullname, FULLNAME_LOG_MAX)
            ),
            Self::NoAddress => "Device has no address".to_string(),
            Self::Dial(e) => e.clone(),
            Self::Superseded => "Replaced by a newer connect request".to_string(),
        }
    }

    /// Whether TIDAL is told the connect failed. A superseded request is not a failure of the
    /// device: the newer request owns the outcome, and telling TIDAL would clear its pick. A
    /// malformed one names no device, so there is no pick of TIDAL's to release or clear.
    pub(crate) fn is_announced(&self) -> bool {
        !matches!(self, Self::Superseded | Self::Malformed(_))
    }
}

/// The discovered device under `fullname`, waiting up to `deadline` for the browse to resolve it.
async fn find_device(
    ctrl: &Mutex<TidalConnectController>,
    fullname: &str,
    deadline: Duration,
) -> Option<MdnsDevice> {
    // Subscribed in the lock hold that does the first lookup: a device resolved after it
    // bumps the receiver, one resolved before it is already in the list.
    let (mut changes, mut found) = {
        let guard = ctrl.lock().unwrap_or_else(|e| e.into_inner());
        (guard.watch_devices(), guard.discovered_device(fullname))
    };
    let wait = async {
        loop {
            if let Some(device) = found.take() {
                return Some(device);
            }
            changes.changed().await.ok()?;
            found = ctrl
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .discovered_device(fullname);
        }
    };
    tokio::time::timeout(deadline, wait).await.unwrap_or(None)
}

/// Dial the device `requested` names; a request naming a device mDNS never found dials nothing.
/// `disable_tls` is for tests.
pub(crate) async fn dial_requested(
    ctrl: &Mutex<TidalConnectController>,
    requested: &MdnsDevice,
    resolve_deadline: Duration,
    disable_tls: bool,
    events: mpsc::Sender<WsClientEvent>,
) -> Result<(WsClient, MdnsDevice), ConnectFailure> {
    let device = find_device(ctrl, &requested.fullname, resolve_deadline)
        .await
        .ok_or_else(|| ConnectFailure::Unresolved(requested.fullname.clone()))?;
    let address = device.addresses.first().ok_or(ConnectFailure::NoAddress)?;
    let ws = WsClient::connect(address, device.port, disable_tls, events)
        .await
        .map_err(|e| ConnectFailure::Dial(e.to_string()))?;
    Ok((ws, device))
}

#[cfg(test)]
#[path = "../../../tests/unit/connect/controller/dial.rs"]
mod tests;
