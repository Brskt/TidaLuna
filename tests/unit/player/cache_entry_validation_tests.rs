//! Tests for `src/player/mod.rs`, attached to it by `#[path]`.

use super::{CacheReadError, looks_like_media, looks_like_mpeg_frame, read_cache_entry};

fn flac() -> Vec<u8> {
    let mut v = b"fLaC\x00\x00\x00\x22".to_vec();
    v.extend_from_slice(&[0u8; 32]);
    v
}

#[test]
fn known_containers_are_accepted() {
    assert!(looks_like_media(&flac()));

    let mut mp4 = vec![0, 0, 0, 0x20];
    mp4.extend_from_slice(b"ftypM4A ");
    mp4.extend_from_slice(&[0u8; 16]);
    assert!(looks_like_media(&mp4));

    let mut ogg = b"OggS".to_vec();
    ogg.extend_from_slice(&[0u8; 32]);
    assert!(looks_like_media(&ogg));
}

/// The case that matters: a key that no longer matches the stored ciphertext
/// decrypts without error and yields noise. XOR-ing a real header stands in
/// for that - AES-CTR is exactly an XOR against a keystream.
#[test]
fn bytes_decrypted_with_the_wrong_key_are_rejected() {
    let mut noise = flac();
    for (i, b) in noise.iter_mut().enumerate() {
        *b ^= 0x5A_u8.wrapping_add(i as u8);
    }
    assert!(!looks_like_media(&noise));
}

#[test]
fn a_truncated_entry_is_rejected() {
    assert!(!looks_like_media(b""));
    assert!(!looks_like_media(b"fLaC"));
}

/// A 7-byte ADTS header: MPEG-4 AAC-LC, 44.1 kHz, stereo, `frame_len` bytes long.
fn adts_header(frame_len: usize) -> [u8; 7] {
    [
        0xFF,
        0xF1,                                    // 12-bit sync, MPEG-4, layer 00, no CRC
        0x50,                                    // AAC-LC, sampling index 4
        0x80 | ((frame_len >> 11) & 0x03) as u8, // channel config 2 + length high bits
        ((frame_len >> 3) & 0xFF) as u8,
        (((frame_len & 0x07) << 5) as u8) | 0x1F,
        0xFC,
    ]
}

fn padded(head: &[u8], len: usize) -> Vec<u8> {
    let mut v = head.to_vec();
    v.resize(len, 0);
    v
}

#[test]
fn a_real_adts_frame_is_accepted_when_its_successor_is_there() {
    let mut data = padded(&adts_header(32), 32);
    data.extend_from_slice(&adts_header(32));
    assert!(looks_like_mpeg_frame(&data));
    assert!(looks_like_media(&data));
}

/// Noise can pass the field checks; it will not also place a sync word exactly
/// where the length field says.
#[test]
fn an_adts_frame_with_nothing_at_its_successor_is_rejected() {
    assert!(!looks_like_mpeg_frame(&padded(&adts_header(32), 64)));
}

#[test]
fn an_adts_header_stands_alone_when_the_frame_runs_past_what_we_have() {
    assert!(looks_like_mpeg_frame(&padded(&adts_header(4096), 64)));
}

#[test]
fn adts_headers_with_reserved_fields_are_rejected() {
    let mut reserved_rate = adts_header(32);
    reserved_rate[2] = 0x74; // sampling index 13
    assert!(!looks_like_mpeg_frame(&padded(&reserved_rate, 64)));

    let mut no_channels = adts_header(32);
    no_channels[3] = 0x00; // channel configuration 0
    assert!(!looks_like_mpeg_frame(&padded(&no_channels, 64)));
}

#[test]
fn a_real_mp3_frame_is_accepted() {
    // MPEG-1 Layer III, 128 kbps, 44.1 kHz.
    assert!(looks_like_mpeg_frame(&padded(
        &[0xFF, 0xFB, 0x90, 0xC0],
        32
    )));
}

/// The whole point of the rewrite: a bare sync word used to be enough, and 1 random
/// byte pair in 2048 has one.
#[test]
fn a_bare_sync_word_is_no_longer_enough() {
    assert!(!looks_like_mpeg_frame(&padded(
        &[0xFF, 0xE0, 0x90, 0xC0],
        32
    )));
}

#[test]
fn mpeg_headers_with_reserved_or_invalid_fields_are_rejected() {
    // Version 01 is reserved.
    assert!(!looks_like_mpeg_frame(&padded(
        &[0xFF, 0xEB, 0x90, 0xC0],
        32
    )));
    // Bitrate index 1111 is invalid, 0000 is the free format.
    assert!(!looks_like_mpeg_frame(&padded(
        &[0xFF, 0xFB, 0xF0, 0xC0],
        32
    )));
    assert!(!looks_like_mpeg_frame(&padded(
        &[0xFF, 0xFB, 0x00, 0xC0],
        32
    )));
    // Sampling index 11 is reserved.
    assert!(!looks_like_mpeg_frame(&padded(
        &[0xFF, 0xFB, 0x9C, 0xC0],
        32
    )));
}

fn entry(dir: &std::path::Path, bytes: &[u8]) -> std::path::PathBuf {
    let path = dir.join("entry");
    std::fs::write(&path, bytes).unwrap();
    path
}

/// The distinction the whole enum exists for: key-side failures must not condemn
/// a file that may be perfectly good.
#[test]
fn an_unusable_key_keeps_the_entry_instead_of_condemning_it() {
    let tmp = tempfile::tempdir().unwrap();
    let path = entry(tmp.path(), &flac());
    assert!(matches!(
        read_cache_entry(&path, "not a key id"),
        Err(CacheReadError::Unreadable(_))
    ));
}

#[test]
fn a_missing_file_is_reported_as_an_orphaned_row() {
    let tmp = tempfile::tempdir().unwrap();
    assert!(matches!(
        read_cache_entry(&tmp.path().join("gone"), ""),
        Err(CacheReadError::Orphaned)
    ));
}

#[test]
fn bytes_that_are_not_a_container_condemn_the_entry() {
    let tmp = tempfile::tempdir().unwrap();
    let path = entry(tmp.path(), &[0x11; 64]);
    assert!(matches!(
        read_cache_entry(&path, ""),
        Err(CacheReadError::Corrupt(_))
    ));
}

#[test]
fn an_unencrypted_container_reads_back_unchanged() {
    let tmp = tempfile::tempdir().unwrap();
    let path = entry(tmp.path(), &flac());
    assert_eq!(read_cache_entry(&path, "").ok(), Some(flac()));
}
