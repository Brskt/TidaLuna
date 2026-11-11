//! Tests for `src/plugins/store/storage.rs`, attached to it by `#[path]`.

use super::*;
use crate::plugins::store::tests::{mem, stored_row};

const TINY: StorageLimits = StorageLimits {
    max_value_bytes: 8,
    max_key_bytes: 4,
    max_namespace_keys: 2,
    max_namespace_bytes: 10,
};

#[test]
fn quota_rejects_an_oversized_value_or_key() {
    for (key, value, limit) in [("k", "123456789", "per-value"), ("toolong", "v", "per-key")] {
        let mut conn = mem();
        let err = enforce_storage_quota(&mut conn, "ns", key, value, &TINY).unwrap_err();
        assert!(err.contains(limit), "key {key:?}: got: {err}");
    }
}

#[test]
fn quota_caps_namespace_key_count_but_allows_replace() {
    let mut conn = mem();
    stored_row(&mut conn, "ns", "Ns", "{}");
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
    stored_row(&mut conn, "ns", "Ns", "{}");
    storage_set(&mut conn, "ns", "a", "12345").expect("seed"); // 5 bytes, cap 10
    // A new key pushing the namespace total to 11 bytes is rejected...
    let err = enforce_storage_quota(&mut conn, "ns", "b", "123456", &TINY).unwrap_err();
    assert!(err.contains("byte limit"), "got: {err}");
    // ...exactly at the cap (5 + 5 = 10) is allowed.
    enforce_storage_quota(&mut conn, "ns", "b", "12345", &TINY).expect("at cap allowed");
}
