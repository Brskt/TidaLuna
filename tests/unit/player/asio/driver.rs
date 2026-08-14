//! Tests for `src/player/asio/driver.rs`, attached to it by `#[path]`.

use super::*;

#[test]
fn parses_a_braced_clsid() {
    let row = parse_driver_row("RME ASIO", "{12345678-1234-1234-1234-123456789ABC}").unwrap();
    assert_eq!(row.name, "RME ASIO");
    assert_eq!(row.clsid, 0x12345678_1234_1234_1234_123456789ABC);
}

#[test]
fn parses_a_braceless_lowercase_clsid() {
    let row = parse_driver_row("ASIO4ALL v2", "abcdef01-2345-6789-abcd-ef0123456789").unwrap();
    assert_eq!(row.clsid, 0xABCDEF01_2345_6789_ABCD_EF0123456789);
}

#[test]
fn rejects_malformed_clsid() {
    // Too few groups.
    assert!(parse_driver_row("x", "{12345678-1234}").is_none());
    // Wrong group length.
    assert!(parse_driver_row("x", "{1234567-1234-1234-1234-123456789ABC}").is_none());
    // Non-hex digit.
    assert!(parse_driver_row("x", "{1234567G-1234-1234-1234-123456789ABC}").is_none());
    // Empty.
    assert!(parse_driver_row("x", "").is_none());
}

#[test]
fn rejects_empty_name() {
    assert!(parse_driver_row("   ", "{12345678-1234-1234-1234-123456789ABC}").is_none());
}

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

#[test]
fn an_explicit_request_never_falls_through_to_another_driver() {
    let drivers = installed();
    let picked = open_candidates(&drivers, Some("Focusrite USB ASIO"));
    assert_eq!(picked.len(), 1);
    assert_eq!(picked[0].clsid, 2);
}

#[test]
fn a_request_is_matched_after_trimming() {
    let drivers = installed();
    let picked = open_candidates(&drivers, Some("  Realtek ASIO "));
    assert_eq!(picked.len(), 1);
    assert_eq!(picked[0].clsid, 3);
}

#[test]
fn no_request_keeps_every_driver_in_enumeration_order() {
    let drivers = installed();
    let picked = open_candidates(&drivers, None);
    assert_eq!(
        picked.iter().map(|d| d.clsid).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
}

#[test]
fn a_request_naming_no_installed_driver_keeps_every_candidate() {
    // What the device list sends today: a shared-mode device name, which no ASIO
    // driver is called. Falling through to the open attempts is the point.
    let drivers = installed();
    let picked = open_candidates(&drivers, Some("Speakers (4- Focusrite USB Audio)"));
    assert_eq!(picked.len(), 3);
}

#[test]
fn nothing_installed_yields_no_candidate() {
    assert!(open_candidates(&[], Some("Focusrite USB ASIO")).is_empty());
    assert!(open_candidates(&[], None).is_empty());
}
