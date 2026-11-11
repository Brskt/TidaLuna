//! Tests for `src/ipc/plugin/download/name.rs`, attached to it by `#[path]`.

use super::*;

#[test]
fn a_destination_name_is_made_safe_for_every_filesystem_or_refused() {
    for (path, expected) in [
        (
            "/music/A: B? C* D\" E<F>G|H.flac",
            Some("A_ B_ C_ D_ E_F_G_H.flac"),
        ),
        ("/m/Song. ", Some("Song")),
        ("/m/aux.flac", Some("_aux.flac")),
        ("/m/...", None),
        ("/", None),
    ] {
        match expected {
            Some(name) => assert_eq!(sanitized_name(path), name, "{path:?}"),
            None => assert!(sanitized_destination(path).is_err(), "{path:?}"),
        }
    }
}

fn sanitized_name(path: &str) -> String {
    sanitized_destination(path)
        .unwrap()
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .to_string()
}

/// A byte budget, not a character count: 300 CJK characters are 900 bytes; a cut that
/// counted characters would still hand the filesystem a name it refuses.
#[test]
fn a_multibyte_name_is_bounded_in_bytes_on_a_char_boundary() {
    let out = sanitized_name(&format!("/m/{}.flac", "曲".repeat(300)));
    assert!(out.len() <= MAX_NAME_BYTES, "{} bytes", out.len());
    assert!(out.ends_with(".flac"));
    // 250 bytes of stem budget is 83 whole characters, not 83 and a third.
    assert_eq!(out.chars().filter(|c| *c == '曲').count(), 83);
}

#[test]
fn an_extension_filling_the_budget_leaves_no_stem_to_keep() {
    let out = sanitized_name(&format!("/m/s.{}", "e".repeat(400)));
    assert!(out.len() <= MAX_NAME_BYTES, "{} bytes", out.len());
}

#[test]
fn a_trailing_space_exposed_by_the_cut_is_removed() {
    let out = sanitized_name(&format!("/m/{} {}", "a".repeat(254), "b".repeat(50)));
    assert_eq!(out.len(), 254);
    assert!(!out.ends_with(' '));
}

#[test]
fn only_the_final_extension_names_a_download_audio() {
    assert!(audio_named(std::path::Path::new("/m/Song.M4A")));
    assert!(audio_named(std::path::Path::new("/m/a.zip.flac")));
    assert!(!audio_named(std::path::Path::new("/m/a.flac.zip")));
    assert!(!audio_named(std::path::Path::new("/m/.m4a")));
}
