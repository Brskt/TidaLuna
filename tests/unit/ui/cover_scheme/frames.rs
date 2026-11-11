//! Tests for `src/ui/cover_scheme/frames.rs`, attached by `#[path]`.

use super::*;
use crate::ui::cover_scheme::store::put_refusal;

/// The page decides by the status alone: 502 asks again soon, 503 waits for the codec, 507
/// asks at the next miss, and anything else spends one of two attempts. A transient answered as
/// spent freezes a playable cover; a fault answered as transient retries it without end.
#[test]
fn each_failure_reaches_the_page_as_the_status_that_says_what_to_do() {
    for (class, status) in [
        (FailureClass::Network, 502),
        (FailureClass::Refused, 422),
        (FailureClass::Corrupt, 422),
        (FailureClass::Unloadable, 503),
        (FailureClass::Disk, 500),
    ] {
        assert_eq!(frames_refusal(class).0, status, "{class:?}");
    }
    for (kind, status) in [
        (std::io::ErrorKind::StorageFull, 507),
        (std::io::ErrorKind::PermissionDenied, 500),
    ] {
        assert_eq!(put_refusal(&kind.into()).0, status, "{kind:?}");
    }
}
