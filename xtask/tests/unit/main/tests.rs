//! Tests for `xtask/src/main.rs`, attached to it by `#[path]`.

use super::numeric_version;

#[test]
fn drops_prerelease_suffix() {
    assert_eq!(numeric_version("0.0.4-alpha", 4), "0.0.4.0");
    assert_eq!(numeric_version("1.2.3-rc.1", 4), "1.2.3.0");
    assert_eq!(numeric_version("0.0.4-alpha", 3), "0.0.4");
}

#[test]
fn drops_build_metadata() {
    assert_eq!(numeric_version("1.2.3+build.42", 4), "1.2.3.0");
}

#[test]
fn pads_missing_parts() {
    assert_eq!(numeric_version("1", 4), "1.0.0.0");
    assert_eq!(numeric_version("1.2", 4), "1.2.0.0");
    assert_eq!(numeric_version("1.2", 3), "1.2.0");
}

#[test]
fn truncates_extra_parts() {
    assert_eq!(numeric_version("1.2.3.4.5", 4), "1.2.3.4");
    assert_eq!(numeric_version("1.2.3.4", 3), "1.2.3");
}

#[test]
fn passes_through_already_normal() {
    assert_eq!(numeric_version("1.2.3.4", 4), "1.2.3.4");
    assert_eq!(numeric_version("1.2.3", 3), "1.2.3");
}

#[test]
fn handles_garbage_suffix_per_part() {
    assert_eq!(numeric_version("1abc.2def.3ghi", 4), "1.2.3.0");
}
