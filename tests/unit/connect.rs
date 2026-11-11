//! Tests for `src/connect/mod.rs`, attached to it by `#[path]`.

use super::*;

/// TIDAL names the account before the first discovery builds the controller, and again when
/// another account logs in; dropping either leaves a cast on no account or the previous one.
#[test]
fn the_account_reaches_the_controller_before_and_after_it_exists() {
    let mut cm = ConnectManager::new();
    cm.set_session_credential("1234");
    assert!(cm.controller().is_none(), "storing must not build it");

    let ctrl = cm.install_controller(TidalConnectController::new(None));
    assert_eq!(ctrl.lock().unwrap().session_credential(), Some("1234"));

    cm.set_session_credential("5678");
    assert_eq!(ctrl.lock().unwrap().session_credential(), Some("5678"));
}
