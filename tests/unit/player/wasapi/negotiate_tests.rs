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

/// A source is never re-offered its own width as a container: a 16-bit one asks for 32-bit first.
#[test]
fn a_source_is_offered_wider_containers_first_and_never_a_no_op_re_offer() {
    for (source_bps, expected) in [
        (16, vec!["32c/16v Int", "24c/24v Int", "32c/32v Float"]),
        (24, vec!["32c/24v Int", "16c/16v Int", "32c/32v Float"]),
        (
            32,
            vec!["32c/32v Int", "24c/24v Int", "16c/16v Int", "32c/32v Float"],
        ),
    ] {
        assert_eq!(labels(source_bps), expected, "{source_bps}-bit source");
    }
}

/// 0x8889000E is the permanent class: taken for a transient lock it would keep retrying a mode
/// the user disabled, and the reverse demotes a mode that would work. 0x8889000F must fall
/// through to the next candidate: read as a lock it would abandon formats the device accepts.
#[test]
fn the_two_error_classifiers_each_claim_only_their_own_codes() {
    for (text, in_use, mode_disabled) in [
        ("AUDCLNT_E_DEVICE_IN_USE", true, false),
        ("Windows returned an error: 0x8889000A", true, false),
        ("windows returned an error: 0x8889000a", true, false),
        ("error: 0x8889000A", true, false),
        ("error: 0x8889000E", false, true),
        ("error: 0x8889000F", false, false),
    ] {
        assert_eq!(is_device_in_use_error(text), in_use, "{text}");
        assert_eq!(
            is_exclusive_mode_disabled_error(text),
            mode_disabled,
            "{text}"
        );
    }
}
