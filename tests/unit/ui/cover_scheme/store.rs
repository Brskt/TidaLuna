//! Tests for `src/ui/cover_scheme/store.rs`, attached by `#[path]`.

use super::*;

/// The sizes are the page's; their sum sets the allocation, capped before anything is copied.
#[test]
fn an_upload_over_the_cap_is_refused_before_it_is_allocated() {
    for (sizes, max, expected) in [(vec![3, 4], 8, Ok(7)), (vec![5, 4], 8, Err(413))] {
        assert_eq!(
            body_size(sizes.clone(), max).map_err(|(status, _)| status),
            expected,
            "{sizes:?} under {max}"
        );
    }
}
