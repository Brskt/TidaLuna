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
    let mut converted = Vec::new();
    let idr = annex_b_access_unit(&sample, &params(), &mut converted).unwrap();
    assert!(idr);
    let mut out = Vec::new();
    access_unit(&params().prefix().unwrap(), idr, &converted, &mut out);

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
    let mut converted = vec![0xFF; 8];
    let idr = annex_b_access_unit(&[0, 0, 0, 2, 0x41, 0x9A], &params(), &mut converted).unwrap();
    assert!(!idr);
    let mut out = vec![0xFF; 8];
    access_unit(&params().prefix().unwrap(), idr, &converted, &mut out);
    assert_eq!(out, [0, 0, 0, 1, 0x41, 0x9A]);
}

/// A malformed sample must error rather than reach the decoder half-converted.
#[test]
fn a_malformed_nal_framing_is_refused() {
    let mut out = Vec::new();
    for (length_size, sample, what) in [
        (LengthSize::FourBytes, &[0, 0, 0, 0, 0x41][..], "length 0"),
        (
            LengthSize::FourBytes,
            &[0, 0, 0, 2, 0x41, 0x9A, 0, 0][..],
            "cut length field",
        ),
        (
            LengthSize::FourBytes,
            &[0, 0, 0, 1, 0xC1][..],
            "forbidden_zero_bit",
        ),
        (
            LengthSize::OneByte,
            &[2, 0x41, 0x9A, 3, 0x41][..],
            "1-byte overrun",
        ),
    ] {
        let p = ParameterSets {
            length_size,
            ..params()
        };
        assert!(annex_b_access_unit(sample, &p, &mut out).is_err(), "{what}");
    }
}

#[test]
fn every_nal_of_a_sample_is_written_and_any_idr_among_them_counts() {
    // The IDR first: a NAL after it must not take its keyframe away.
    let expected = [0, 0, 0, 1, 0x65, 0x88, 0, 0, 0, 1, 0x41, 0x9A];
    for (length_size, sample) in [
        (LengthSize::OneByte, &[2, 0x65, 0x88, 2, 0x41, 0x9A][..]),
        (
            LengthSize::TwoBytes,
            &[0, 2, 0x65, 0x88, 0, 2, 0x41, 0x9A][..],
        ),
    ] {
        let p = ParameterSets {
            length_size,
            ..params()
        };
        let mut out = Vec::new();
        assert!(annex_b_access_unit(sample, &p, &mut out).unwrap());
        assert_eq!(out, expected);
    }
    let mut out = vec![0xFF; 4];
    assert!(!annex_b_access_unit(&[], &params(), &mut out).unwrap());
    assert!(out.is_empty(), "an empty sample leaves nothing behind");
}

/// An `avcC` with no SPS or no PPS describes a stream that cannot be started. Better to say
/// so here than to hand the decoder slices it can never interpret.
#[test]
fn an_avcc_without_both_parameter_sets_is_refused() {
    let mut p = params();
    p.sps.clear();
    assert!(p.prefix().is_err());

    let mut p = params();
    p.pps.clear();
    assert!(p.prefix().is_err());
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
    let sps = vec![SPS_16X16.to_vec()];
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

/// Both ways in front of the decoder pass every NAL through `admit_nal`: a subset SPS in the SPS
/// list, a start code hidden in the PPS list, and an SVC slice in a sample are each refused.
#[test]
fn a_hostile_nal_is_refused_on_both_ways_in() {
    let sps = vec![SPS_16X16.to_vec()];
    let pps = vec![vec![0x68, 0xCE, 0x3C, 0x80]];
    let mut subset = SPS_16X16.to_vec();
    subset[0] = 0x6F;
    assert!(parameter_sets(&avcc(3, vec![SPS_16X16.to_vec(), subset], pps)).is_err());
    let smuggling = vec![vec![0x68, 0xCE, 0, 0, 0, 1, 0x67]];
    assert!(parameter_sets(&avcc(3, sps, smuggling)).is_err());

    let mut out = Vec::new();
    let sample = [0, 0, 0, 2, 0x65, 0x88, 0, 0, 0, 4, 0x74, 0x80, 0x80, 0x88];
    assert!(annex_b_access_unit(&sample, &params(), &mut out).is_err());
}

/// One sample is one decode call: past 128 full-frame slices it is refused before the decoder sees
/// any of them.
#[test]
fn a_sample_asking_more_than_one_call_may_do_is_refused() {
    let p = ParameterSets {
        length_size: LengthSize::OneByte,
        ..params()
    };
    let mut out = Vec::new();
    assert!(annex_b_access_unit(&[2u8, 0x65, 0x88].repeat(129), &p, &mut out).is_err());
}

/// Baseline, level 3.0, one macroblock.
const SPS_16X16: [u8; 6] = [0x67, 0x42, 0x00, 0x1E, 0xDA, 0x79];

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
