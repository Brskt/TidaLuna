//! Tests for `src/player/asio/convert.rs`, attached to it by `#[path]`.

use super::*;

#[test]
fn samples_are_written_byte_exact_to_every_integer_container() {
    for (sample, src_bps, container, expected) in [
        // 16-bit passthrough.
        (0x1234, 16, AsioSampleType::Int16, &[0x34, 0x12][..]),
        // Widening shifts a right-justified sample left: 0x1234 << 8, LE.
        (0x1234, 16, AsioSampleType::Int24, &[0x00, 0x34, 0x12][..]),
        // And 0x1234 << 16, LE.
        (
            0x1234,
            16,
            AsioSampleType::Int32,
            &[0x00, 0x00, 0x34, 0x12][..],
        ),
        // 24-bit source passthrough, three packed bytes.
        (0x123456, 24, AsioSampleType::Int24, &[0x56, 0x34, 0x12][..]),
        // Negative 16-bit source widened into 24-bit: -1 << 8 = 0xFFFFFF00.
        (-1, 16, AsioSampleType::Int24, &[0x00, 0xFF, 0xFF][..]),
    ] {
        let mut out = vec![0u8; expected.len()];
        write_dst_sample(sample, src_bps, container, &mut out);
        assert_eq!(
            out, expected,
            "{sample:#x} from {src_bps} bits into {container:?}"
        );
    }
}

#[test]
fn float32_normalizes_full_scale_to_unit() {
    let mut out = [0u8; 4];
    write_dst_sample(32767, 16, AsioSampleType::Float32, &mut out);
    let pos = f32::from_le_bytes(out);
    assert!((pos - 32767.0 / 32768.0).abs() < 1e-6);

    write_dst_sample(-32768, 16, AsioSampleType::Float32, &mut out);
    let neg = f32::from_le_bytes(out);
    assert!((neg + 1.0).abs() < 1e-6);
}

#[test]
fn gain_attenuates_below_unity_and_passes_through_at_unity() {
    // 0.5 gain on full-scale 16-bit positive: 32767 * 0.5 -> 16383 (trunc).
    let mut atten = [0u8; 2];
    write_dst_sample(
        apply_gain(32767, 0.5),
        16,
        AsioSampleType::Int16,
        &mut atten,
    );
    assert_eq!(i16::from_le_bytes(atten), 16383);
    // Unity is a pure passthrough.
    assert_eq!(apply_gain(0x1234, 1.0), 0x1234);
}

#[test]
fn maps_supported_asio_sample_types_only() {
    assert_eq!(AsioSampleType::from_asio(16), Some(AsioSampleType::Int16));
    assert_eq!(AsioSampleType::from_asio(17), Some(AsioSampleType::Int24));
    assert_eq!(AsioSampleType::from_asio(18), Some(AsioSampleType::Int32));
    assert_eq!(AsioSampleType::from_asio(19), Some(AsioSampleType::Float32));
    // MSB / Float64 / DSD are unsupported -> the channel is refused.
    assert_eq!(AsioSampleType::from_asio(0), None); // Int16MSB
    assert_eq!(AsioSampleType::from_asio(20), None); // Float64LSB
    assert_eq!(AsioSampleType::from_asio(32), None); // DSD
}
