//! Tests for `src/video/cover.rs`, attached by `#[path]`.

use super::*;

/// A hostile table can name an offset no machine could address: the end it adds up to overflows,
/// and is refused rather than panicking. A zero-length sample carries no NAL, and an empty slice
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

use shiguredo_mp4::bitstream::h264::LengthSize;

fn params() -> ParameterSets {
    ParameterSets {
        sps: vec![vec![0x67, 0x42, 0x00, 0x1E]],
        pps: vec![vec![0x68, 0xCE, 0x3C, 0x80]],
        length_size: LengthSize::FourBytes,
    }
}

/// The decoder cannot start without them, and an MP4 keeps them in `avcC` rather than in the
/// stream.
#[test]
fn a_keyframe_is_prefixed_with_its_parameter_sets() {
    let sample = [0, 0, 0, 2, 0x65, 0xAA];
    let mut out = Vec::new();
    annex_b_access_unit(&sample, &params(), true, &mut out).unwrap();

    assert_eq!(
        out,
        [
            0, 0, 0, 1, 0x67, 0x42, 0x00, 0x1E, 0, 0, 0, 1, 0x68, 0xCE, 0x3C, 0x80, 0, 0, 0, 1,
            0x65, 0xAA,
        ]
    );
}

#[test]
fn a_non_keyframe_is_written_alone_over_a_cleared_buffer() {
    let mut out = vec![0xFF; 8];
    annex_b_access_unit(&[0, 0, 0, 1, 0x41], &params(), false, &mut out).unwrap();
    assert_eq!(out, [0, 0, 0, 1, 0x41]);
}

/// A malformed sample must error rather than reach the decoder half-converted.
#[test]
fn a_sample_whose_length_overruns_is_refused() {
    let sample = [0, 0, 0, 9, 0x41];
    let mut out = Vec::new();
    assert!(annex_b_access_unit(&sample, &params(), false, &mut out).is_err());
}

/// An `avcC` with no SPS or no PPS describes a stream that cannot be started. Better to say
/// so here than to hand the decoder slices it can never interpret.
#[test]
fn an_avcc_without_both_parameter_sets_is_refused() {
    let mut p = params();
    p.sps.clear();
    let mut out = Vec::new();
    assert!(annex_b_access_unit(&[0, 0, 0, 1, 0x65], &p, true, &mut out).is_err());

    let mut p = params();
    p.pps.clear();
    assert!(annex_b_access_unit(&[0, 0, 0, 1, 0x65], &p, true, &mut out).is_err());
}

use shiguredo_mp4::Uint;

/// `parameter_sets` reads only `length_size_minus_one`, `sps_list` and `pps_list`; the other
/// fields take their absent-optional shape.
fn avcc(length_size_minus_one: u8, sps: Vec<Vec<u8>>, pps: Vec<Vec<u8>>) -> AvccBox {
    AvccBox {
        avc_profile_indication: 0x42,
        profile_compatibility: 0x00,
        avc_level_indication: 0x1E,
        length_size_minus_one: Uint::new(length_size_minus_one),
        sps_list: sps,
        pps_list: pps,
        chroma_format: None,
        bit_depth_luma_minus8: None,
        bit_depth_chroma_minus8: None,
        sps_ext_list: Vec::new(),
    }
}

/// `parameter_sets` itself must refuse the reserved length-prefix width, not just the crate
/// function it calls (an `unwrap_or(FourBytes)` regression would pass silently), and the narrower
/// widths must come through too: four bytes is also what a reader falls back on.
#[test]
fn parameter_sets_refuses_the_reserved_width_and_carries_the_legal_ones_through() {
    let sps = vec![vec![0x67, 0x42, 0x00, 0x1E]];
    let pps = vec![vec![0x68, 0xCE, 0x3C, 0x80]];
    for (minus_one, expected) in [
        (2, None),
        (3, Some(LengthSize::FourBytes)),
        (0, Some(LengthSize::OneByte)),
        (1, Some(LengthSize::TwoBytes)),
    ] {
        let sets = parameter_sets(&avcc(minus_one, sps.clone(), pps.clone()));
        let Some(width) = expected else {
            assert!(sets.is_err(), "length_size_minus_one {minus_one}");
            continue;
        };
        let sets = sets.unwrap();
        assert_eq!(sets.length_size, width, "length_size_minus_one {minus_one}");
        assert_eq!(sets.sps, sps);
        assert_eq!(sets.pps, pps);
    }
}

/// A label-boundary match, never a substring (both lookalikes are registrable). The host comes
/// from the parsed url, plaintext is refused outright, and the static `images/` covers never pass.
#[test]
fn only_an_https_video_url_on_the_artwork_host_is_a_cover_url() {
    for (url, expected) in [
        (
            "https://resources.tidal.com/videos/a/b/c/1280x1280.mp4",
            true,
        ),
        (
            "https://resources.tidal.com.evil.example/videos/a.mp4",
            false,
        ),
        ("https://evil-resources.tidal.com/videos/a.mp4", false),
        (
            "https://resources.tidal.com@evil.example/videos/a.mp4",
            false,
        ),
        ("http://resources.tidal.com/videos/a.mp4", false),
        ("not a url", false),
        (
            "https://resources.tidal.com/images/a/b/c/1280x1280.jpg",
            false,
        ),
    ] {
        assert_eq!(is_cover_url(url), expected, "{url}");
    }
}

/// Not a decode test: decoding needs Cisco's library, which is downloaded at runtime and is
/// not available here. What is testable without it is that a file which is not an MP4 is
/// refused before any decoder is asked for. A file that ends after its `ftyp` carries no track
/// and must be refused the same way rather than panic.
#[test]
fn a_file_that_is_not_an_mp4_is_refused_before_a_decoder_is_needed() {
    assert!(demux_cover_samples(b"this is not an mp4").is_err());

    let mut truncated = Vec::new();
    truncated.extend_from_slice(&[0, 0, 0, 0x14]);
    truncated.extend_from_slice(b"ftypisom");
    truncated.extend_from_slice(&[0, 0, 0x02, 0x00]);
    truncated.extend_from_slice(b"isom");
    assert!(demux_cover_samples(&truncated).is_err());
}

/// A decode timestamp that does not fit a signed value, or sits at the signed boundary beside a
/// positive composition offset, is refused rather than overflowing an unchecked addition, which
/// panics in a debug build.
#[test]
fn a_presentation_time_is_computed_or_refused_never_wrapped() {
    for (timestamp, offset, expected) in [
        (u64::MAX, None, None),
        (i64::MAX as u64, Some(1), None),
        (1_000, Some(200), Some(1_200)),
        (1_000, None, Some(1_000)),
    ] {
        assert_eq!(
            presentation_time(timestamp, offset).ok(),
            expected,
            "{timestamp} + {offset:?}"
        );
    }
}

/// Driven on `SampleBudget` directly: an MP4 under `MAX_COVER_SAMPLES` bytes meets the byte
/// ceiling first, and a fixture past it would be a hand-built file of that size.
#[test]
fn a_sample_count_at_the_ceiling_refuses_the_next_charge() {
    // Generous enough that the byte ceiling cannot be what refuses: this asserts the count.
    let mut budget = SampleBudget::new(usize::MAX);
    for _ in 0..MAX_COVER_SAMPLES {
        budget.charge(1).unwrap();
    }

    assert!(budget.charge(1).is_err());
}

#[test]
fn samples_claiming_more_than_the_file_holds_are_refused() {
    let mut budget = SampleBudget::new(1_000);

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
    let mut budget = SampleBudget::new(1_000);
    budget.charge(1).unwrap();

    assert!(budget.charge(usize::MAX).is_err());
}

fn decoded_frame(width: usize, height: usize) -> CoverFrame {
    CoverFrame {
        width,
        height,
        y: Vec::new(),
        u: Vec::new(),
        v: Vec::new(),
        y_stride: width,
        u_stride: width / 2,
        v_stride: width / 2,
        pts_micros: 0,
    }
}

/// The first frame is what the header will state; it has nothing to disagree with.
#[test]
fn a_frame_that_changes_resolution_mid_stream_is_refused() {
    for (frames, width, height, accepted) in [
        (vec![decoded_frame(640, 640)], 640, 640, true),
        (vec![decoded_frame(640, 640)], 640, 480, false),
        (vec![decoded_frame(640, 640)], 320, 640, false),
        (Vec::new(), 640, 640, true),
    ] {
        assert_eq!(
            refuse_resolution_change(&frames, width, height).is_ok(),
            accepted,
            "{width}x{height} after {} frames",
            frames.len()
        );
    }
}

/// The five sizes TIDAL publishes for an album cover are 80, 160, 320, 640 and 1280 on a side,
/// and only the last one cannot be decoded whole. These pin the boundary in both directions.
const COVER_BASE: &str = "https://resources.tidal.com/videos/fc890b4a/ec2c/4729/9431/bcd88b1eedac";

#[test]
fn a_variant_at_or_under_the_cap_is_left_alone() {
    for edge in [80, 160, 320, 640, 1280]
        .into_iter()
        .filter(|&edge| edge <= MAX_COVER_EDGE)
    {
        assert_eq!(
            capped_cover_url(&format!("{COVER_BASE}/{edge}x{edge}.mp4")),
            None,
            "{edge}x{edge} is already at or under the cap and must not be rewritten"
        );
    }
}

#[test]
fn a_segment_that_is_not_a_square_variant_is_passed_through() {
    for last in [
        "1280x720.mp4",
        "cover.mp4",
        "1280x1280",
        "x.mp4",
        "1280x.mp4",
    ] {
        assert_eq!(
            capped_cover_url(&format!("{COVER_BASE}/{last}")),
            None,
            "{last} is not a square <n>x<n>.mp4 and must be left as it came"
        );
    }
}

#[test]
fn a_url_that_does_not_parse_is_refused_rather_than_panicking() {
    assert_eq!(capped_cover_url("not a url at all"), None);
}

#[test]
fn two_oversized_variants_of_one_cover_key_the_same_job() {
    let at_cap = format!("{COVER_BASE}/{MAX_COVER_EDGE}x{MAX_COVER_EDGE}.mp4");
    let header = canonical_cover_url(&format!("{COVER_BASE}/1280x1280.mp4"));
    let capped = canonical_cover_url(&at_cap);
    assert_eq!(header, capped);
    assert_eq!(header, at_cap);

    // A smaller variant keeps its own key.
    let small = format!("{COVER_BASE}/320x320.mp4");
    assert_eq!(canonical_cover_url(&small), small);
}

#[test]
fn a_cover_is_wanted_until_its_last_requester_leaves() {
    let interest = CoverInterest::new();
    assert!(
        !interest.abandoned(),
        "the requester that started it is waiting"
    );

    assert!(interest.try_join(), "a second frame may share the work");
    interest.release();
    assert!(
        !interest.abandoned(),
        "one of two requesters leaving must not stop work the other is waiting for"
    );

    interest.release();
    assert!(interest.abandoned(), "the last one out ends the job");
}

/// Pins the one-way door at zero: a plain increment in place of the compare-and-swap would hand
/// the latecomer an abandoned answer, a 499 for a frame still on screen.
#[test]
fn a_job_nobody_wants_cannot_be_joined_by_a_latecomer() {
    let interest = CoverInterest::new();
    interest.release();
    assert!(interest.abandoned());

    assert!(
        !interest.try_join(),
        "a job past its last requester must refuse a latecomer rather than revive"
    );
    assert!(
        interest.abandoned(),
        "and the refused join must not have moved the count"
    );
}

#[test]
fn a_share_given_back_twice_cannot_wrap_into_interest() {
    let interest = CoverInterest::new();
    interest.release();
    interest.release();
    interest.release();
    assert!(interest.abandoned());
    assert!(!interest.try_join());
}

fn frame() -> CoverFrame {
    CoverFrame {
        width: 2,
        height: 2,
        y: vec![0; 4],
        u: vec![0; 1],
        v: vec![0; 1],
        y_stride: 2,
        u_stride: 1,
        v_stride: 1,
        pts_micros: 0,
    }
}

fn sample(pts_micros: i64) -> CoverSample {
    CoverSample {
        annex_b: Vec::new(),
        pts_micros,
    }
}

#[test]
fn presentation_times_are_paired_in_display_order() {
    let mut frames = vec![frame(), frame(), frame()];
    // Decode order for an I-P-B pattern: the P frame arrives before the B frame it brackets.
    let samples = [sample(0), sample(66_666), sample(33_333)];
    assign_presentation_times(&mut frames, &samples).unwrap();

    let times: Vec<i64> = frames.iter().map(|f| f.pts_micros).collect();
    assert_eq!(times, [0, 33_333, 66_666]);
}

#[test]
fn a_frame_count_that_cannot_be_paired_is_refused() {
    let mut frames = vec![frame(), frame()];
    let samples = [sample(0), sample(33_333), sample(66_666)];
    assert!(assign_presentation_times(&mut frames, &samples).is_err());
}
