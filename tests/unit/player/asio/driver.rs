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
