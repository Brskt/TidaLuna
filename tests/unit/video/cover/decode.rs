//! Tests for `src/video/cover/decode.rs`, attached by `#[path]`.

use super::*;

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

/// A stream that reorders emits pictures in display order while its samples arrive in decode
/// order. Every frame must draw one time, in order, from a first at zero, and a frame beyond the
/// sample count is refused.
#[test]
fn frame_times_are_sorted_rebased_and_one_per_sample() {
    // Decode order for an I-P-B pattern starting late, as TIDAL's covers do.
    let mut times = PresentationTimes::new(vec![33_333, 100_000, 66_666]);
    assert_eq!(times.span, 66_667);
    let drawn: Vec<i64> = (0..3).map(|_| times.next().unwrap()).collect();
    assert_eq!(drawn, [0, 33_333, 66_667]);
    assert!(times.next().is_err(), "a fourth frame has no sample");
}
