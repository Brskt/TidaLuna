//! Tests for `src/util/mp4_samples.rs`, attached by `#[path]`.

use super::*;

/// A hostile table can name an offset no machine could address: the end it adds up to overflows,
/// and is refused rather than panicking. A zero-length sample carries nothing, and an empty slice
/// would read as a decode failure further down.
#[test]
fn a_sample_is_returned_only_when_wholly_inside_the_buffer() {
    let file = [0u8, 1, 2, 3, 4, 5, 6, 7];

    for (offset, size, expected) in [
        (2, 3, Some(2usize..5)),
        // Ending exactly on the last byte is still inside.
        (5, 3, Some(5..8)),
        (6, 9, None),
        (8, 1, None),
        (u64::MAX, 1, None),
        (u64::MAX - 1, 4, None),
        (1, 0, None),
    ] {
        assert_eq!(
            sample_bytes(&file, offset, size),
            expected.map(|range| &file[range]),
            "{offset}+{size}"
        );
    }
}

#[test]
fn a_sample_count_at_the_ceiling_refuses_the_next_charge() {
    // Generous enough that the byte ceiling cannot be what refuses: this asserts the count.
    let mut budget = SampleBudget::new("track", 3, usize::MAX);
    for _ in 0..3 {
        budget.charge(1).unwrap();
    }

    assert!(budget.charge(1).is_err());
}

#[test]
fn samples_claiming_more_than_the_file_holds_are_refused() {
    let mut budget = SampleBudget::new("track", usize::MAX, 1_000);

    // Disjoint samples summing to the file's own length are exactly what a well-formed file
    // looks like. The last legal byte must still be accepted.
    budget.charge(600).unwrap();
    budget.charge(400).unwrap();

    // One byte past it can only mean two samples pointing at the same place.
    let result = budget.charge(1);

    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("of a 1000 byte file"),
        "the refusal should name the file it was measured against",
    );
}

/// A declared size wide enough to wrap the running total would otherwise slip under the ceiling
/// by arithmetic rather than by being small. The total starts above zero: on an empty budget
/// `usize::MAX` fits without wrapping and proves nothing about the addition.
#[test]
fn a_sample_size_that_would_overflow_the_running_total_is_refused() {
    let mut budget = SampleBudget::new("track", usize::MAX, 1_000);
    budget.charge(1).unwrap();

    assert!(budget.charge(usize::MAX).is_err());
}
