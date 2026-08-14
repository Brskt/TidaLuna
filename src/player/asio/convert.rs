//! Pure PCM conversion for the ASIO output backend.
//!
//! `write_dst_sample` mirrors `wasapi::convert_pcm_frame`'s byte math; ASIO output is
//! bit-identical to the exclusive WASAPI path; the RT host deinterleaves inline into the
//! per-channel buffers. Platform-independent, and unit-tested on any host.
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
#[path = "../../../tests/unit/player/asio/convert.rs"]
mod tests;
