//! Tests for `src/audio/mp4_tags.rs`, attached by `#[path]`.

use super::*;

/// The children of a box body, as `(type, body)`.
fn children(mut body: &[u8]) -> Vec<([u8; 4], &[u8])> {
    let mut out = Vec::new();
    while body.len() >= 8 {
        let size = u32::from_be_bytes(body[..4].try_into().unwrap()) as usize;
        out.push((body[4..8].try_into().unwrap(), &body[8..size]));
        body = &body[size..];
    }
    assert!(body.is_empty(), "trailing bytes after the last box");
    out
}

/// Each `ilst` item as `(atom or freeform name, data type, value)`, in file order.
fn items(udta_body: &[u8]) -> Vec<(String, u32, Vec<u8>)> {
    let [(meta_type, meta)] = children(udta_body)[..] else {
        panic!("one meta box")
    };
    assert_eq!(&meta_type, b"meta");
    let meta = children(&meta[4..]);
    assert_eq!(
        meta.iter().map(|(t, _)| *t).collect::<Vec<_>>(),
        [*b"hdlr", *b"ilst"]
    );
    assert_eq!(&meta[0].1[8..12], b"mdir", "metadata handler");
    children(meta[1].1)
        .into_iter()
        .map(|(kind, item)| {
            let parts = children(item);
            let (_, data) = parts.last().unwrap();
            let type_code = u32::from_be_bytes(data[..4].try_into().unwrap());
            let name = if &kind == b"----" {
                assert_eq!(&parts[0].1[4..], b"com.apple.iTunes");
                String::from_utf8(parts[1].1[4..].to_vec()).unwrap()
            } else {
                latin1(&kind)
            };
            (name, type_code, data[8..].to_vec())
        })
        .collect()
}

/// Atom types are Latin-1: 0xA9 is the copyright sign iTunes names its own atoms with.
fn latin1(kind: &[u8]) -> String {
    kind.iter().map(|&b| char::from(b)).collect()
}

fn comments(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// What upstream's `node-taglib-sharp` writes for the same tags, field for field: a reader that
/// shows the track right after a FLAC download must show this one the same. A wrong atom or format
/// is silent until someone opens the file in another player.
#[test]
fn tags_land_in_the_atoms_and_formats_upstream_writes() {
    let tags = comments(&[
        ("TITLE", "Song"),
        ("ARTIST", "A"),
        ("ARTIST", "B"),
        ("ALBUMARTIST", "A"),
        ("ALBUM", "Record"),
        ("YEAR", "2024"),
        ("DATE", "2019-05-01"),
        ("COPYRIGHT", "(C) Label"),
        ("COMMENT", "https://tidal.com/track/1"),
        ("GENRE", "Rock"),
        ("TRACKNUMBER", "3"),
        ("TOTALTRACKS", "12"),
        ("DISCNUMBER", "2"),
        ("BPM", "120"),
        ("LYRICS", "la la"),
        ("ISRC", "USABC2400001"),
        ("MUSICBRAINZ_TRACKID", "mb-track"),
        ("MUSICBRAINZ_ALBUMID", "mb-album"),
        ("REPLAYGAIN_TRACK_GAIN", "-6.5432"),
        ("REPLAYGAIN_TRACK_PEAK", "0.98"),
        // Upstream writes neither.
        ("UPC", "0123"),
        ("ORGANIZATION", "Label"),
    ]);
    let cover = Picture {
        mime: "image/jpeg".into(),
        data: vec![0xff, 0xd8, 0xff],
    };

    let got = items(&udta_body(&tags, Some(&cover)).unwrap());

    let text = |name: &[u8], value: &str| (latin1(name), 1, value.as_bytes().to_vec());
    assert_eq!(
        got,
        [
            text(b"\xa9nam", "Song"),
            text(b"\xa9ART", "A; B"),
            text(b"aART", "A"),
            text(b"\xa9alb", "Record"),
            text(b"\xa9day", "2024"),
            text(b"cprt", "(C) Label"),
            text(b"\xa9cmt", "https://tidal.com/track/1"),
            text(b"\xa9gen", "Rock"),
            ("trkn".into(), 0, vec![0, 0, 0, 3, 0, 12, 0, 0]),
            ("disk".into(), 0, vec![0, 0, 0, 2, 0, 0, 0, 0]),
            ("tmpo".into(), 21, vec![0, 120]),
            text(b"\xa9lyr", "la la"),
            text(b"ISRC", "USABC2400001"),
            text(b"MusicBrainz Track Id", "mb-track"),
            text(b"MusicBrainz Album Id", "mb-album"),
            text(b"REPLAYGAIN_TRACK_GAIN", "-6.54 dB"),
            text(b"REPLAYGAIN_TRACK_PEAK", "0.980000"),
            ("covr".into(), 13, vec![0xff, 0xd8, 0xff]),
        ]
    );
}
