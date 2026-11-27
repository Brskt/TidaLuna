//! TIDAL's loudness normalization, applied the way its SDK does: a per-track factor on top of the
//! listener's level, never inside it. The page computes each factor with the same formula
//! (`normalizeVolume`); Rust only picks one by the live mode and multiplies.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering::Relaxed};

use serde_json::Value;

/// The "Normalize volume" setting as TIDAL stores it: `NONE`, `ALBUM` or `TRACK`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum NormalizationMode {
    Off = 0,
    Album = 1,
    Track = 2,
}

impl NormalizationMode {
    /// Anything but the two named modes is off, which plays every track untouched.
    pub(crate) fn from_wire(value: Option<&Value>) -> Self {
        match value.and_then(Value::as_str) {
            Some("ALBUM") => Self::Album,
            Some("TRACK") => Self::Track,
            _ => Self::Off,
        }
    }

    fn from_bits(bits: u8) -> Self {
        match bits {
            1 => Self::Album,
            2 => Self::Track,
            _ => Self::Off,
        }
    }
}

/// The live mode, shared by the player thread and every output path. A toggle reaches the
/// playing track at the next buffer.
#[derive(Clone, Default, Debug)]
pub(crate) struct Normalization(Arc<AtomicU8>);

impl Normalization {
    pub(crate) fn mode(&self) -> NormalizationMode {
        NormalizationMode::from_bits(self.0.load(Relaxed))
    }

    pub(crate) fn set(&self, mode: NormalizationMode) {
        self.0.store(mode as u8, Relaxed);
    }
}

/// A track's two loudness factors, fixed for the track: they come from its replay gains.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TrackGains {
    album: f32,
    track: f32,
}

impl TrackGains {
    /// What a track with no factor plays at: a Connect load, or a sender that sends none.
    pub const UNITY: Self = Self {
        album: 1.0,
        track: 1.0,
    };

    pub(crate) fn new(album: f32, track: f32) -> Self {
        Self { album, track }
    }

    pub(crate) fn from_wire(album: Option<&Value>, track: Option<&Value>) -> Self {
        Self::new(factor_from_wire(album), factor_from_wire(track))
    }

    /// Both factors in one word, for a real-time reader that cannot lock: never a torn pair.
    #[cfg(target_os = "windows")]
    pub(crate) fn to_bits(self) -> u64 {
        (u64::from(self.album.to_bits()) << 32) | u64::from(self.track.to_bits())
    }

    #[cfg(target_os = "windows")]
    pub(crate) fn from_bits(bits: u64) -> Self {
        Self::new(
            f32::from_bits((bits >> 32) as u32),
            f32::from_bits(bits as u32),
        )
    }

    pub(crate) fn factor(self, mode: NormalizationMode) -> f32 {
        match mode {
            NormalizationMode::Off => 1.0,
            NormalizationMode::Album => self.album,
            NormalizationMode::Track => self.track,
        }
    }
}

/// A factor only attenuates (`min(10^((4 + gain) / 20), 1)`). Anything outside (0, 1]
/// is a bad value, and a bad value plays the track untouched rather than silencing it.
pub(crate) fn factor_from_wire(value: Option<&Value>) -> f32 {
    match value.and_then(Value::as_f64) {
        Some(f) if f.is_finite() && f > 0.0 && f <= 1.0 => f as f32,
        _ => 1.0,
    }
}

#[cfg(test)]
#[path = "../../tests/unit/player/normalization.rs"]
mod tests;
