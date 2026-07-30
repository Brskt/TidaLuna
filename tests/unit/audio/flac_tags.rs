//! Tests for `src/audio/flac_tags.rs`, attached to it by `#[path]`.

use super::*;

/// Magic, a last-flagged 34-byte STREAMINFO, then one byte standing in for audio.
fn minimal_flac() -> Vec<u8> {
    let mut v = b"fLaC".to_vec();
    v.push(0x80);
    v.extend_from_slice(&[0x00, 0x00, 0x22]);
    v.extend_from_slice(&[0u8; 34]);
    v.push(0xFF);
    v
}

fn comments() -> Vec<(String, String)> {
    vec![
        ("TITLE".to_string(), "Song".to_string()),
        ("ARTIST".to_string(), "One".to_string()),
        ("ARTIST".to_string(), "Two".to_string()),
    ]
}

/// Walk the whole chain rather than stopping at the first flagged block, so an
/// uncleared STREAMINFO flag cannot hide behind an early return.
fn chain(data: &[u8]) -> Vec<(u8, bool, Vec<u8>)> {
    let mut out = Vec::new();
    let mut i = 4;
    loop {
        let header = data[i];
        let len = u32::from_be_bytes([0, data[i + 1], data[i + 2], data[i + 3]]) as usize;
        out.push((
            header & 0x7F,
            header & 0x80 != 0,
            data[i + 4..i + 4 + len].to_vec(),
        ));
        i += 4 + len;
        if header & 0x80 != 0 {
            return out;
        }
    }
}

#[test]
fn streaminfo_stays_first_and_the_audio_survives() {
    let out = retag(minimal_flac(), &comments(), None).unwrap();
    assert_eq!(&out[..4], b"fLaC");
    assert_eq!(chain(&out)[0].0, 0, "STREAMINFO must stay the first block");
    assert_eq!(*out.last().unwrap(), 0xFF, "audio frames must survive");
}

/// The regression this guards is a STREAMINFO that keeps the flag it arrived with,
/// which yields a file some decoders accept and others reject.
#[test]
fn only_the_final_block_is_flagged_last() {
    let out = retag(minimal_flac(), &comments(), None).unwrap();
    let blocks = chain(&out);
    let flagged: Vec<usize> = blocks
        .iter()
        .enumerate()
        .filter(|(_, b)| b.1)
        .map(|(i, _)| i)
        .collect();
    assert_eq!(flagged, vec![blocks.len() - 1]);
}

/// Decoded by hand as little endian, so swapping the encoder's byte order fails here
/// rather than passing a test that only counts blocks.
#[test]
fn comment_entries_round_trip_as_little_endian() {
    let out = retag(minimal_flac(), &comments(), None).unwrap();
    let body = chain(&out)
        .into_iter()
        .find(|b| b.0 == VORBIS_COMMENT)
        .expect("a comment block")
        .2;

    let vendor_len = u32::from_le_bytes(body[0..4].try_into().unwrap()) as usize;
    let mut at = 4 + vendor_len;
    let count = u32::from_le_bytes(body[at..at + 4].try_into().unwrap()) as usize;
    at += 4;

    let mut seen = Vec::new();
    for _ in 0..count {
        let len = u32::from_le_bytes(body[at..at + 4].try_into().unwrap()) as usize;
        at += 4;
        seen.push(String::from_utf8(body[at..at + len].to_vec()).unwrap());
        at += len;
    }
    assert_eq!(seen, vec!["TITLE=Song", "ARTIST=One", "ARTIST=Two"]);
}

/// Decoded by hand as big endian, the opposite of the comment block above.
#[test]
fn picture_fields_round_trip_as_big_endian() {
    let pic = Picture {
        mime: "image/jpeg".to_string(),
        data: vec![9, 8, 7, 6],
    };
    let out = retag(minimal_flac(), &comments(), Some(pic)).unwrap();
    let body = chain(&out)
        .into_iter()
        .find(|b| b.0 == PICTURE)
        .expect("a picture block")
        .2;

    assert_eq!(
        u32::from_be_bytes(body[0..4].try_into().unwrap()),
        FRONT_COVER
    );
    let mime_len = u32::from_be_bytes(body[4..8].try_into().unwrap()) as usize;
    assert_eq!(&body[8..8 + mime_len], b"image/jpeg");
    let data_len = u32::from_be_bytes(body[body.len() - 8..body.len() - 4].try_into().unwrap());
    assert_eq!(data_len, 4);
    assert_eq!(&body[body.len() - 4..], &[9, 8, 7, 6]);
}

/// Without this, a track tagged twice grows a duplicate header on every pass.
#[test]
fn a_second_pass_replaces_the_blocks_instead_of_stacking_them() {
    let pic = || Picture {
        mime: "image/png".to_string(),
        data: vec![1, 2, 3, 4],
    };
    let once = retag(minimal_flac(), &comments(), Some(pic())).unwrap();
    let twice = retag(once, &comments(), Some(pic())).unwrap();
    let blocks = chain(&twice);
    assert_eq!(blocks.iter().filter(|b| b.0 == VORBIS_COMMENT).count(), 1);
    assert_eq!(blocks.iter().filter(|b| b.0 == PICTURE).count(), 1);
}

#[test]
fn malformed_streams_are_refused_without_panicking() {
    assert!(retag(b"nope".to_vec(), &comments(), None).is_err());
    assert!(retag(b"fLaC".to_vec(), &comments(), None).is_err());
    // A declared length that runs past the buffer.
    let mut runaway = b"fLaC".to_vec();
    runaway.push(0x80);
    runaway.extend_from_slice(&[0xFF, 0xFF, 0xFF]);
    assert!(retag(runaway, &comments(), None).is_err());
}
