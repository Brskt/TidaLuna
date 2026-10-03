//! Tests for the video codec setting in `src/settings.rs`, attached by `#[path]`.

use super::fixture::mem_conn;
use super::*;

#[test]
fn the_video_codec_is_off_by_default() {
    let mut conn = mem_conn();
    assert!(!load_video_codec_enabled(&mut conn));
}

#[test]
fn save_then_load_the_video_codec_switch_roundtrips() {
    let mut conn = mem_conn();
    save_video_codec_enabled(&mut conn, true);
    assert!(load_video_codec_enabled(&mut conn));
    save_video_codec_enabled(&mut conn, false);
    assert!(!load_video_codec_enabled(&mut conn));
}

/// The fallback decides whether a third-party download happens.
#[test]
fn an_unparseable_stored_video_codec_value_falls_back_to_off() {
    let mut conn = mem_conn();
    set(&conn, VIDEO_CODEC_ENABLED, "yes-please");
    assert!(!load_video_codec_enabled(&mut conn));
}

#[test]
fn boot_settings_carry_the_video_codec_switch() {
    let mut conn = mem_conn();
    save_video_codec_enabled(&mut conn, true);
    let boot = load_boot_settings(&mut conn);
    assert!(boot.video_codec_enabled);
}
