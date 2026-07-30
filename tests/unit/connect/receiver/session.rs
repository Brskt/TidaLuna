//! Tests for `src/connect/receiver/session.rs`, attached to it by `#[path]`.

use super::*;

#[test]
fn session_id_is_none_on_entropy_failure() {
    assert_eq!(generate_session_id_with(|_| false), None);
}

#[test]
fn session_id_is_uuid_v4_shaped() {
    let id = generate_session_id_with(|buf| {
        for (i, b) in buf.iter_mut().enumerate() {
            *b = i as u8;
        }
        true
    })
    .expect("RNG ok");
    let parts: Vec<&str> = id.split('-').collect();
    assert_eq!(id.len(), 36);
    assert_eq!(parts.len(), 5);
    assert!(parts[2].starts_with('4'), "version nibble must be 4");
    assert!(
        matches!(parts[3].chars().next(), Some('8'..='b')),
        "variant nibble must be 8..b"
    );
}
