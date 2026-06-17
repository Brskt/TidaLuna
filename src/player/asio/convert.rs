//! Pure PCM conversion for the ASIO output backend.
//!
//! `write_dst_sample` mirrors `wasapi::convert_pcm_frame`'s byte math so ASIO output is
//! bit-identical to the exclusive WASAPI path; the RT host deinterleaves inline into the
//! per-channel buffers. Platform-independent, so unit-tested on any host.
#![cfg_attr(not(target_os = "windows"), allow(dead_code))]

/// The ASIO output sample types we support (a subset of `ASIOSampleType`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum AsioSampleType {
    Int16,
    Int24,
    Int32,
    Float32,
}

impl AsioSampleType {
    /// Bytes one sample occupies in an ASIO buffer. Int24 is 3 packed bytes, not
    /// 24 bits in a 32-bit word.
    pub(crate) fn bytes_per_sample(self) -> usize {
        match self {
            AsioSampleType::Int16 => 2,
            AsioSampleType::Int24 => 3,
            AsioSampleType::Int32 | AsioSampleType::Float32 => 4,
        }
    }

    /// Map a driver's `ASIOSampleType` code to a type we render, or `None` for
    /// unsupported formats (MSB, Float64, DSD).
    pub(crate) fn from_asio(code: i32) -> Option<AsioSampleType> {
        match code {
            16 => Some(AsioSampleType::Int16),   // ASIOSTInt16LSB
            17 => Some(AsioSampleType::Int24),   // ASIOSTInt24LSB
            18 => Some(AsioSampleType::Int32),   // ASIOSTInt32LSB
            19 => Some(AsioSampleType::Float32), // ASIOSTFloat32LSB
            _ => None,
        }
    }
}

/// Apply digital gain to a right-justified sample; unity is a bit-perfect passthrough.
pub(crate) fn apply_gain(sample: i32, gain: f32) -> i32 {
    if gain < 1.0 {
        ((sample as f32) * gain) as i32
    } else {
        sample
    }
}

/// Write one right-justified i32 sample into `out` as the ASIO sample type.
/// Mirrors the destination arms of `wasapi::convert_pcm_frame`.
pub(crate) fn write_dst_sample(sample: i32, src_bps: u32, dst: AsioSampleType, out: &mut [u8]) {
    match dst {
        AsioSampleType::Int16 => {
            let val = match src_bps {
                16 => sample as i16,
                24 => (sample >> 8) as i16,
                32 => (sample >> 16) as i16,
                _ => 0,
            };
            out[..2].copy_from_slice(&val.to_le_bytes());
        }
        AsioSampleType::Int24 => {
            let val = match src_bps {
                16 => sample << 8,
                24 => sample,
                32 => sample >> 8,
                _ => 0,
            };
            out[0] = (val & 0xFF) as u8;
            out[1] = ((val >> 8) & 0xFF) as u8;
            out[2] = ((val >> 16) & 0xFF) as u8;
        }
        AsioSampleType::Int32 => {
            let val = match src_bps {
                16 => sample << 16,
                24 => sample << 8,
                32 => sample,
                _ => 0,
            };
            out[..4].copy_from_slice(&val.to_le_bytes());
        }
        AsioSampleType::Float32 => {
            let max_val = (1i64 << (src_bps - 1)) as f32;
            let f = (sample as f32) / max_val;
            out[..4].copy_from_slice(&f.to_le_bytes());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn int16_passthrough_is_bit_perfect() {
        let mut out = [0u8; 2];
        write_dst_sample(0x1234, 16, AsioSampleType::Int16, &mut out);
        assert_eq!(out, [0x34, 0x12]);
    }

    #[test]
    fn widening_shifts_left_for_right_justified_samples() {
        // 16-bit source 0x1234 widened to 24-bit and 32-bit containers.
        let mut out24 = [0u8; 3];
        write_dst_sample(0x1234, 16, AsioSampleType::Int24, &mut out24);
        assert_eq!(out24, [0x00, 0x34, 0x12]); // 0x1234 << 8 = 0x123400, LE

        let mut out32 = [0u8; 4];
        write_dst_sample(0x1234, 16, AsioSampleType::Int32, &mut out32);
        assert_eq!(out32, [0x00, 0x00, 0x34, 0x12]); // 0x1234 << 16, LE
    }

    #[test]
    fn int24_is_three_packed_bytes_and_handles_negatives() {
        // 24-bit source passthrough (0x123456 right-justified).
        let mut out = [0u8; 3];
        write_dst_sample(0x123456, 24, AsioSampleType::Int24, &mut out);
        assert_eq!(out, [0x56, 0x34, 0x12]);
        // negative 16-bit source (-1) widened into 24-bit: -1 << 8 = 0xFFFFFF00.
        let mut neg = [0u8; 3];
        write_dst_sample(-1, 16, AsioSampleType::Int24, &mut neg);
        assert_eq!(neg, [0x00, 0xFF, 0xFF]);
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
}
