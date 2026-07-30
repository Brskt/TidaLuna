//! Tests for `src/connect/controller/mod.rs`, attached to it by `#[path]`.

use super::*;
use crate::connect::types::DeviceType;

fn dev(fullname: &str) -> MdnsDevice {
    MdnsDevice {
        addresses: vec!["127.0.0.1".into()],
        friendly_name: "n".into(),
        fullname: fullname.into(),
        id: "id".into(),
        port: 1,
        device_type: DeviceType::TidalConnect,
    }
}

#[test]
fn insert_device_capped_dedups_and_bounds() {
    let mut devices = Vec::new();
    insert_device_capped(&mut devices, dev("a"), 2);
    insert_device_capped(&mut devices, dev("b"), 2);
    // Dedup by fullname: re-seeing a known device does not grow the list.
    insert_device_capped(&mut devices, dev("a"), 2);
    assert_eq!(devices.len(), 2);
    // At the cap, a new distinct device is refused.
    insert_device_capped(&mut devices, dev("c"), 2);
    assert_eq!(devices.len(), 2);
    assert!(devices.iter().all(|d| d.fullname != "c"));
}
