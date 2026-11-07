//! Tests for `src/ipc/plugin/admission.rs`, attached by `#[path]`.

use super::*;

const NOW: Ticket = Ticket { epoch: 3, page: 5 };

/// No plugin line may run while the real token is still in TIDAL's SDK, nor in a page or session
/// the work was not started for. The purge goes first whenever it is still due.
#[test]
fn plugin_code_is_admitted_only_past_the_gate_and_for_the_current_page() {
    for (gate_open, asked, purge_due, expected, why) in [
        (
            false,
            NOW,
            true,
            Err(Closed::Gate),
            "the cold-boot refresh has not rotated the real token yet",
        ),
        (
            true,
            Ticket { epoch: 3, page: 4 },
            false,
            Err(Closed::Stale),
            "a pass of the page before a reload would run in the new one before its pins",
        ),
        (
            true,
            Ticket { epoch: 2, page: 5 },
            false,
            Err(Closed::Stale),
            "a pass of the session before a logout would run in the one after",
        ),
        (
            true,
            NOW,
            true,
            Ok(true),
            "the first admission purges the blob before any plugin",
        ),
        (true, NOW, false, Ok(false), "the blob is already gone"),
    ] {
        assert_eq!(entry(gate_open, NOW, asked, purge_due), expected, "{why}");
    }
}
