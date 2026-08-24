//! Tests for `src/player/wasapi.rs`, attached to it by `#[path]`.
//!
//! Two pure surfaces sit under the exclusive negotiation: the candidate list, whose ORDER
//! decides which format the hardware is asked for first, and the two error classifiers,
//! which read the Display text of a third-party error type. Neither needs a device.

use super::{is_device_in_use_error, is_exclusive_mode_disabled_error, negotiate_format};

const RATE: u32 = 44_100;

fn labels(source_bps: u32) -> Vec<String> {
    negotiate_format(RATE, 2, source_bps)
        .into_iter()
        .map(|(_, _, label)| label)
        .collect()
}

#[test]
fn a_16_bit_source_asks_for_a_32_bit_container_first_and_never_a_16_bit_one() {
    assert_eq!(
        labels(16),
        vec!["32c/16v Int", "24c/24v Int", "32c/32v Float"],
        "a 16-bit container would be a no-op re-offer of the source width"
    );
}

#[test]
fn a_24_bit_source_keeps_the_16_bit_container_and_drops_the_24_bit_one() {
    assert_eq!(
        labels(24),
        vec!["32c/24v Int", "16c/16v Int", "32c/32v Float"]
    );
}

#[test]
fn a_32_bit_source_offers_every_narrower_container() {
    assert_eq!(
        labels(32),
        vec!["32c/32v Int", "24c/24v Int", "16c/16v Int", "32c/32v Float"]
    );
}

#[test]
fn float_is_always_the_last_resort() {
    for bps in [16, 24, 32] {
        assert_eq!(
            labels(bps).last().map(String::as_str),
            Some("32c/32v Float"),
            "an integer container is bit-exact where float is a conversion ({bps}-bit source)"
        );
    }
}

#[test]
fn every_candidate_shares_the_one_20ms_period() {
    let candidates = negotiate_format(RATE, 2, 16);
    // A per-candidate period would make the accepted latency depend on which format the
    // driver happened to take.
    let periods: Vec<i64> = candidates.iter().map(|(_, period, _)| *period).collect();
    assert!(
        periods.windows(2).all(|w| w[0] == w[1]),
        "candidates disagree on the period: {periods:?}"
    );
    // 20 ms in 100ns units, the value the negotiation comments commit to.
    assert_eq!(periods[0], 200_000);
}

#[test]
fn a_device_in_use_error_is_recognised_by_name_and_by_either_case_of_its_code() {
    assert!(is_device_in_use_error("AUDCLNT_E_DEVICE_IN_USE"));
    assert!(is_device_in_use_error(
        "Windows returned an error: 0x8889000A"
    ));
    assert!(is_device_in_use_error(
        "windows returned an error: 0x8889000a"
    ));
}

#[test]
fn the_two_classifiers_never_claim_each_others_codes() {
    // 0x8889000E is the permanent class: taking it for a transient lock would keep
    // retrying a mode the user disabled. The reverse demotes a mode that would work.
    assert!(is_exclusive_mode_disabled_error("error: 0x8889000E"));
    assert!(!is_device_in_use_error("error: 0x8889000E"));
    assert!(!is_exclusive_mode_disabled_error("error: 0x8889000A"));
}

#[test]
fn an_endpoint_create_failure_is_neither() {
    // 0x8889000F (ENDPOINT_CREATE_FAILED) must fall through to the next candidate. Reading
    // it as a lock would abandon formats the device does accept.
    assert!(!is_device_in_use_error("error: 0x8889000F"));
    assert!(!is_exclusive_mode_disabled_error("error: 0x8889000F"));
}
