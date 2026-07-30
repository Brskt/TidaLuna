//! Tests for `src/ipc/plugin/download.rs`, attached to it by `#[path]`.

use super::*;

/// Separators are already directory boundaries by the time this sees them, so only the
/// last segment is cleaned, as upstream's `path.join`, which likewise cannot tell a
/// caller's separator from one out of a track title.
#[test]
fn an_already_split_path_keeps_its_directories() {
    let out = sanitized_destination("/music/AC/DC - Song.flac").unwrap();
    assert_eq!(out, std::path::Path::new("/music/AC/DC - Song.flac"));
}

#[test]
fn characters_windows_refuses_are_replaced() {
    let out = sanitized_destination("/music/A: B? C* D\" E<F>G|H.flac").unwrap();
    assert_eq!(out.file_name().unwrap(), "A_ B_ C_ D_ E_F_G_H.flac");
}

#[test]
fn a_trailing_dot_or_space_is_removed() {
    assert_eq!(
        sanitized_destination("/m/Song. ")
            .unwrap()
            .file_name()
            .unwrap(),
        "Song"
    );
}

#[test]
fn a_reserved_windows_stem_is_prefixed() {
    assert_eq!(
        sanitized_destination("/m/aux.flac")
            .unwrap()
            .file_name()
            .unwrap(),
        "_aux.flac"
    );
}

#[test]
fn a_name_that_sanitizes_to_nothing_is_refused() {
    assert!(sanitized_destination("/m/...").is_err());
    assert!(sanitized_destination("/").is_err());
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

#[test]
fn an_over_long_name_is_bounded_and_keeps_its_extension() {
    let out = sanitized_name(&format!("/m/{}.flac", "a".repeat(400)));
    assert!(out.len() <= MAX_NAME_BYTES, "{} bytes", out.len());
    assert!(out.ends_with(".flac"));
}

/// A byte budget, not a character count: 300 CJK characters are 900 bytes, so a cut that
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
fn genres_becomes_the_singular_field_players_read() {
    assert_eq!(vorbis_field("genres"), "GENRE");
    assert_eq!(vorbis_field("albumArtist"), "ALBUMARTIST");
    assert_eq!(vorbis_field("musicbrainz_trackid"), "MUSICBRAINZ_TRACKID");
}

#[test]
fn field_names_outside_printable_ascii_are_refused() {
    assert!(valid_field_name("TITLE"));
    assert!(!valid_field_name(""));
    assert!(!valid_field_name("TITLE=X"));
    assert!(!valid_field_name("TIT\nLE"));
    // to_uppercase is Unicode aware, so a non-ASCII key can reach here expanded.
    assert!(!valid_field_name("TITLÉ"));
}
