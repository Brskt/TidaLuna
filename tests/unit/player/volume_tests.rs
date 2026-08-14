//! Tests for `Volume` in `src/player/mod.rs`, attached to it by `#[path]`.

use super::*;

#[test]
fn ordinary_levels_pass_through() {
    assert_eq!(Volume::from_percent(0.0).as_percent(), 0.0);
    assert_eq!(Volume::from_percent(42.5).as_percent(), 42.5);
    assert_eq!(Volume::from_percent(100.0).as_percent(), 100.0);
}

#[test]
fn out_of_range_levels_are_clamped() {
    assert_eq!(Volume::from_percent(500.0).as_percent(), 100.0);
    assert_eq!(Volume::from_percent(-1.0).as_percent(), 0.0);
    assert_eq!(Volume::from_percent(-500.0).as_percent(), 0.0);
}

#[test]
fn a_large_finite_level_cannot_reach_infinity_at_the_cast() {
    // The actually-reachable path: an ordinary JSON number, well inside f64, whose
    // `/ 100.0` quotient still exceeds f32::MAX (~3.4e38) and casts to infinity.
    // The threshold is the quotient, not the input: 1e40 divides down to 1e38 and stays
    // finite, which is why this test pins 1e41 rather than the first value that looks big.
    let raw = 1e41_f64;
    assert!(raw.is_finite(), "the input itself is an ordinary f64");
    assert!(
        ((raw / 100.0) as f32).is_infinite(),
        "unsanitized, the cast saturates"
    );

    let gain = (Volume::from_percent(raw).as_percent() / 100.0) as f32;
    assert_eq!(gain, 1.0);
}

#[test]
fn non_finite_levels_sanitize_to_silence_not_full_scale() {
    // `.clamp()` alone would let NaN through unchanged; the finite check is not
    // redundant. Silence is the safe wrong answer here; full scale is not.
    assert_eq!(Volume::from_percent(f64::NAN).as_percent(), 0.0);
    assert_eq!(Volume::from_percent(f64::NEG_INFINITY).as_percent(), 0.0);
    assert_eq!(Volume::from_percent(f64::INFINITY).as_percent(), 0.0);
    assert!(
        f64::NAN.clamp(0.0, 100.0).is_nan(),
        "the reason from_percent cannot be a bare clamp"
    );
}
