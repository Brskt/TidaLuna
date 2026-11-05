//! Tests for `src/video/webm.rs`, attached by `#[path]`.

use super::*;

/// One EBML element: its ID as written (marker bits included), an eight-byte size, its body.
fn el(id: u32, body: &[u8]) -> Vec<u8> {
    let id_bytes = id.to_be_bytes();
    let skip = id_bytes.iter().take_while(|&&b| b == 0).count();
    let mut out = id_bytes[skip..].to_vec();
    out.push(0x01);
    out.extend_from_slice(&(body.len() as u64).to_be_bytes()[1..]);
    out.extend_from_slice(body);
    out
}

fn uint(id: u32, value: u64) -> Vec<u8> {
    el(id, &value.to_be_bytes())
}

/// What a fixture varies from the file the page's muxer writes.
struct Fixture {
    doc_type: &'static str,
    codec: &'static str,
    width: u64,
    height: u64,
    duration_ms: f64,
    tracks: usize,
}

const GOOD: Fixture = Fixture {
    doc_type: "webm",
    codec: "V_VP9",
    width: 640,
    height: 640,
    duration_ms: 10_033.333,
    tracks: 1,
};

fn webm(f: &Fixture) -> Vec<u8> {
    let header = [
        uint(0x4286, 1),
        uint(0x42F7, 1),
        uint(0x42F2, 4),
        uint(0x42F3, 8),
        el(0x4282, f.doc_type.as_bytes()),
        uint(0x4287, 4),
        uint(0x4285, 2),
    ]
    .concat();
    let info = el(
        0x1549_A966,
        &[
            uint(0x2A_D7B1, 1_000_000),
            el(0x4489, &f.duration_ms.to_be_bytes()),
            // Required by the reader, and written by the page's muxer.
            el(0x4D80, b"TidaLunar"),
            el(0x5741, b"TidaLunar"),
        ]
        .concat(),
    );
    let entries: Vec<u8> = (1..=f.tracks as u64)
        .flat_map(|n| {
            el(
                0xAE,
                &[
                    uint(0xD7, n),
                    uint(0x73C5, n),
                    uint(0x83, 1),
                    el(0x86, f.codec.as_bytes()),
                    el(0xE0, &[uint(0xB0, f.width), uint(0xBA, f.height)].concat()),
                ]
                .concat(),
            )
        })
        .collect();
    let tracks = el(0x1654_AE6B, &entries);
    let cluster = el(
        0x1F43_B675,
        &[
            uint(0xE7, 0),
            el(0xA3, &[0x81, 0, 0, 0x80, 0x82, 0x49, 0x83]),
        ]
        .concat(),
    );
    [
        el(EBML_ID, &header),
        el(0x1853_8067, &[info, tracks, cluster].concat()),
    ]
    .concat()
}

fn expected() -> ExpectedCover {
    ExpectedCover {
        width: 640,
        height: 640,
        duration_micros: 10_033_333,
        tolerance_micros: 34_333,
    }
}

/// The page is untrusted and the file is cached, then played by Chromium: anything but one VP9
/// track of the decoded size and duration, in a WebM, stays out.
#[test]
fn an_upload_that_is_not_a_vp9_webm_of_the_decoded_cover_is_refused() {
    assert_eq!(validate_cover_webm(&webm(&GOOD), &expected()), Ok(()));

    let good = webm(&GOOD);
    for (name, bytes) in [
        ("not EBML", b"this is not a webm file".to_vec()),
        ("truncated", good[..40].to_vec()),
        (
            "matroska",
            webm(&Fixture {
                doc_type: "matroska",
                ..GOOD
            }),
        ),
        (
            "VP8",
            webm(&Fixture {
                codec: "V_VP8",
                ..GOOD
            }),
        ),
        (
            "wrong size",
            webm(&Fixture {
                width: 1280,
                height: 1280,
                ..GOOD
            }),
        ),
        ("two tracks", webm(&Fixture { tracks: 2, ..GOOD })),
        (
            "two frames short",
            webm(&Fixture {
                duration_ms: 9_966.0,
                ..GOOD
            }),
        ),
    ] {
        assert!(
            validate_cover_webm(&bytes, &expected()).is_err(),
            "{name} was accepted"
        );
    }
}
