//! Tests for `src/plugins/store.rs`, attached to it by `#[path]`.

use super::*;

fn mem() -> Connection {
    let mut conn = Connection::open_in_memory().expect("open in-memory db");
    init_schema(&mut conn).expect("init schema");
    conn
}

const TINY: StorageLimits = StorageLimits {
    max_value_bytes: 8,
    max_key_bytes: 4,
    max_namespace_keys: 2,
    max_namespace_bytes: 10,
};

#[test]
fn quota_rejects_oversized_value() {
    let mut conn = mem();
    let err = enforce_storage_quota(&mut conn, "ns", "k", "123456789", &TINY).unwrap_err();
    assert!(err.contains("per-value"), "got: {err}");
}

#[test]
fn quota_rejects_oversized_key() {
    let mut conn = mem();
    let err = enforce_storage_quota(&mut conn, "ns", "toolong", "v", &TINY).unwrap_err();
    assert!(err.contains("per-key"), "got: {err}");
}

#[test]
fn quota_caps_namespace_key_count_but_allows_replace() {
    let mut conn = mem();
    storage_set(&mut conn, "ns", "a", "1").expect("seed a");
    storage_set(&mut conn, "ns", "b", "2").expect("seed b");
    // A new third key exceeds the 2-key limit...
    let err = enforce_storage_quota(&mut conn, "ns", "c", "3", &TINY).unwrap_err();
    assert!(err.contains("key limit"), "got: {err}");
    // ...but replacing an existing key adds no key; it is allowed.
    enforce_storage_quota(&mut conn, "ns", "a", "9", &TINY).expect("replace allowed");
}

#[test]
fn quota_caps_namespace_byte_total() {
    let mut conn = mem();
    storage_set(&mut conn, "ns", "a", "12345").expect("seed"); // 5 bytes, cap 10
    // A new key pushing the namespace total to 11 bytes is rejected...
    let err = enforce_storage_quota(&mut conn, "ns", "b", "123456", &TINY).unwrap_err();
    assert!(err.contains("byte limit"), "got: {err}");
    // ...exactly at the cap (5 + 5 = 10) is allowed.
    enforce_storage_quota(&mut conn, "ns", "b", "12345", &TINY).expect("at cap allowed");
}

#[test]
fn storage_set_roundtrips_under_quota() {
    let mut conn = mem();
    storage_set(&mut conn, "ns", "k", "hello").expect("set");
    assert_eq!(storage_get(&mut conn, "ns", "k").as_deref(), Some("hello"));
}
