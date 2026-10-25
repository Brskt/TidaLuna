//! Tests for `src/player/mod.rs`, attached to it by `#[path]`.

use super::{CacheReadError, looks_like_media, looks_like_mpeg_frame, read_cache_entry};

fn flac() -> Vec<u8> {
    let mut v = b"fLaC\x00\x00\x00\x22".to_vec();
    v.extend_from_slice(&[0u8; 32]);
    v
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

/// An ADTS frame of 32 bytes followed by the next frame's header.
fn followed(header: [u8; 7]) -> Vec<u8> {
    let mut data = padded(&header, 32);
    data.extend_from_slice(&adts_header(32));
    data
}

#[test]
fn only_a_known_container_header_looks_like_media() {
    let mut mp4 = vec![0, 0, 0, 0x20];
    mp4.extend_from_slice(b"ftypM4A ");
    mp4.extend_from_slice(&[0u8; 16]);

    let mut ogg = b"OggS".to_vec();
    ogg.extend_from_slice(&[0u8; 32]);

    // The case that matters: a key that no longer matches the stored ciphertext decrypts without
    // error and yields noise. XOR-ing a real header stands in (AES-CTR is an XOR against a keystream).
    let mut noise = flac();
    for (i, b) in noise.iter_mut().enumerate() {
        *b ^= 0x5A_u8.wrapping_add(i as u8);
    }

    for (what, bytes, expected) in [
        ("flac", flac(), true),
        ("mp4", mp4, true),
        ("ogg", ogg, true),
        ("a real ADTS frame", followed(adts_header(32)), true),
        ("bytes decrypted with the wrong key", noise, false),
        ("an empty entry", b"".to_vec(), false),
        ("a truncated entry", b"fLaC".to_vec(), false),
    ] {
        assert_eq!(looks_like_media(&bytes), expected, "{what}");
    }
}

/// An ADTS header needs valid fields and, when the frame fits what we have, a sync word exactly
/// where its length says (noise can pass the field checks, not that). A bare sync word used to be
/// enough, and one random byte pair in 2048 has one.
#[test]
fn only_a_header_with_valid_fields_looks_like_an_mpeg_frame() {
    let mut reserved_rate = adts_header(32);
    reserved_rate[2] = 0x74; // sampling index 13
    let mut no_channels = adts_header(32);
    no_channels[3] = 0x00; // channel configuration 0

    for (what, data, expected) in [
        (
            "a real ADTS frame with its successor",
            followed(adts_header(32)),
            true,
        ),
        (
            "an ADTS frame with nothing at its successor",
            padded(&adts_header(32), 64),
            false,
        ),
        (
            "an ADTS frame running past what we have",
            padded(&adts_header(4096), 64),
            true,
        ),
        (
            "ADTS with a reserved sampling index",
            followed(reserved_rate),
            false,
        ),
        (
            "ADTS with no channel configuration",
            followed(no_channels),
            false,
        ),
        (
            "an MPEG-1 Layer III frame, 128 kbps, 44.1 kHz",
            padded(&[0xFF, 0xFB, 0x90, 0xC0], 32),
            true,
        ),
        (
            "a bare sync word",
            padded(&[0xFF, 0xE0, 0x90, 0xC0], 32),
            false,
        ),
        (
            "MPEG version 01 is reserved",
            padded(&[0xFF, 0xEB, 0x90, 0xC0], 32),
            false,
        ),
        (
            "bitrate index 1111 is invalid",
            padded(&[0xFF, 0xFB, 0xF0, 0xC0], 32),
            false,
        ),
        (
            "bitrate index 0000 is the free format",
            padded(&[0xFF, 0xFB, 0x00, 0xC0], 32),
            false,
        ),
        (
            "sampling index 11 is reserved",
            padded(&[0xFF, 0xFB, 0x9C, 0xC0], 32),
            false,
        ),
    ] {
        assert_eq!(looks_like_mpeg_frame(&data), expected, "{what}");
    }
}

fn entry(dir: &std::path::Path, bytes: &[u8]) -> std::path::PathBuf {
    let path = dir.join("entry");
    std::fs::write(&path, bytes).unwrap();
    path
}

/// The distinction the whole enum exists for: key-side failures must not condemn
/// a file that may be perfectly good.
#[test]
fn only_bytes_that_are_not_a_container_condemn_a_cache_entry() {
    type Row<'a> = (Option<Vec<u8>>, &'a str, Result<Vec<u8>, &'a str>);
    let rows: [Row; 4] = [
        (Some(flac()), "not a key id", Err("unreadable")),
        (None, "", Err("orphaned")),
        (Some(vec![0x11; 64]), "", Err("corrupt")),
        (Some(flac()), "", Ok(flac())),
    ];
    for (content, key, expected) in rows {
        let tmp = tempfile::tempdir().unwrap();
        let path = match &content {
            Some(bytes) => entry(tmp.path(), bytes),
            None => tmp.path().join("gone"),
        };
        let got = match read_cache_entry(&path, key) {
            Ok(bytes) => Ok(bytes),
            Err(CacheReadError::Unreadable(_)) => Err("unreadable"),
            Err(CacheReadError::Orphaned) => Err("orphaned"),
            Err(CacheReadError::Corrupt(_)) => Err("corrupt"),
        };
        assert_eq!(
            got,
            expected,
            "key {key:?}, file present: {}",
            content.is_some()
        );
    }
}
