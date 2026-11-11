//! Tests for `src/ui/cover_scheme/video.rs`, attached by `#[path]`.

use super::*;
use crate::ui::cover_scheme::SkipVerdict;

/// Chromium places a `bytes=N-` body at N on our scheme without checking a status or a
/// `Content-Range`: CEF's skip to N is acknowledged only over a body that starts there.
#[test]
fn a_range_skip_is_acknowledged_only_over_the_part_that_starts_there() {
    let cover: Vec<u8> = (0..=255).collect();
    let part = ranged_answer(cover.clone(), Some("bytes=100-"));
    assert_eq!(part.body, cover[100..]);
    assert_eq!(part.skip(100), SkipVerdict::Skipped(100));
    assert_eq!(part.skip(99), SkipVerdict::Refused, "another offset");

    for (answer, case) in [
        (ranged_answer(cover.clone(), None), "the whole cover"),
        (ranged_answer(cover, Some("bytes=300-")), "past the end"),
    ] {
        assert_eq!(answer.skip(100), SkipVerdict::Refused, "{case}");
    }
}
