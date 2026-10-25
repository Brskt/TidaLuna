//! Tests for `src/player/asio/driver.rs`, attached to it by `#[path]`.

use super::*;

/// Three drivers installed, only the second one's interface plugged in: the shape that
/// made the app open the Thunderbolt driver and give up.
fn installed() -> Vec<AsioDriverInfo> {
    vec![
        AsioDriverInfo {
            name: "Focusrite Thunderbolt ASIO".to_string(),
            clsid: 1,
        },
        AsioDriverInfo {
            name: "Focusrite USB ASIO".to_string(),
            clsid: 2,
        },
        AsioDriverInfo {
            name: "Realtek ASIO".to_string(),
            clsid: 3,
        },
    ]
}

/// An explicit request naming an installed driver, after trimming, yields that driver alone and
/// never falls through to another one; otherwise every driver stays in enumeration order.
#[test]
fn an_explicit_request_never_falls_through_to_another_driver() {
    let drivers = installed();
    let rows: [(Option<&str>, &[u128]); 4] = [
        (Some("Focusrite USB ASIO"), &[2]),
        (Some("  Realtek ASIO "), &[3]),
        (None, &[1, 2, 3]),
        // What the device list sends today: a shared-mode device name, which no ASIO driver is
        // called. Falling through to the open attempts is the point.
        (Some("Speakers (4- Focusrite USB Audio)"), &[1, 2, 3]),
    ];
    for (request, expected) in rows {
        let picked: Vec<u128> = open_candidates(&drivers, request)
            .iter()
            .map(|d| d.clsid)
            .collect();
        assert_eq!(picked, expected, "{request:?}");
    }
}
