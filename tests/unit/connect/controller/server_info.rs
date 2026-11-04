//! Tests for `src/connect/controller/server_info.rs`, attached to it by `#[path]`.

use super::*;

fn owed(delivery: Option<Delivery>) -> Delivery {
    delivery.expect("owed")
}

fn is_held(delivery: Option<Delivery>) -> bool {
    delivery.is_none()
}

/// `setAuthentication` is callable by any plugin in the page: a device is handed a token once,
/// and again only when what it holds differs (a new token, quality or connection).
#[test]
fn a_device_is_handed_the_same_token_once() {
    let mut ledger = ServerInfoLedger::default();
    owed(ledger.claim(1, "a", None));
    assert!(is_held(ledger.claim(1, "a", None)));

    owed(ledger.claim(1, "b", None));
    assert!(is_held(ledger.claim(1, "b", None)));

    owed(ledger.claim(1, "b", Some("HIGH")));
    assert!(is_held(ledger.claim(1, "b", Some("HIGH"))));

    owed(ledger.claim(2, "b", Some("HIGH")));
}

/// A skipped delivery is a token the device never gets: a send that failed, or a device that
/// reported its server info failing, must not leave the record saying it was handed over.
#[test]
fn a_failed_send_or_a_device_error_owes_the_next_delivery() {
    let mut ledger = ServerInfoLedger::default();
    let first = owed(ledger.claim(1, "a", None));
    ledger.forget_sent();
    let after_error = owed(ledger.claim(1, "a", None));

    // A failure reported late takes nothing back from the delivery that replaced it, even one
    // for the same content.
    ledger.release(&first);
    assert!(is_held(ledger.claim(1, "a", None)));
    ledger.release(&after_error);
    owed(ledger.claim(1, "a", None));
}
