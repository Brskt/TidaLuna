//! Tests for `src/player/mod.rs`, attached to it by `#[path]`.

use super::MediaErrorCode;

#[test]
fn codes_match_the_sdk_map() {
    // Locked to nativePlayer.ts `mediaErrorCodeMap` (no_such_file -> NPO01,
    // unreadable_file -> NPO03): any other wire string reaches the SDK as
    // `errorCode: undefined`.
    assert_eq!(MediaErrorCode::NoSuchFile.as_str(), "no_such_file");
    assert_eq!(MediaErrorCode::UnreadableFile.as_str(), "unreadable_file");
}
