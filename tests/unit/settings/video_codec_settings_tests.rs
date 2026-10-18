//! Tests for the video codec setting in `src/settings.rs`, attached by `#[path]`.

use super::fixture::mem_conn;
use super::*;

#[test]
fn boot_settings_carry_the_video_codec_switch() {
    let mut conn = mem_conn();
    // `false`, the value the default cannot produce: a loader that ignored the row would pass `true`.
    save_video_codec_enabled(&mut conn, false);
    let boot = load_boot_settings(&mut conn);
    assert!(!boot.video_codec_enabled);

    // And back: a loader that answered `false` whatever the row said would pass the first half.
    save_video_codec_enabled(&mut conn, true);
    assert!(load_boot_settings(&mut conn).video_codec_enabled);
}
