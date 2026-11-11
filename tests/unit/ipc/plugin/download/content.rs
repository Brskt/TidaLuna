//! Tests for `src/ipc/plugin/download/content.rs`, attached to it by `#[path]`.

use super::*;

#[test]
fn field_names_outside_printable_ascii_are_refused() {
    assert!(valid_field_name("TITLE"));
    assert!(!valid_field_name(""));
    assert!(!valid_field_name("TITLE=X"));
    assert!(!valid_field_name("TIT\nLE"));
    // to_uppercase is Unicode aware: a non-ASCII key can reach here expanded.
    assert!(!valid_field_name("TITLÉ"));
}

/// An `ftyp` box followed by one box that accounts for the rest.
fn mp4(payload: usize) -> Vec<u8> {
    let mut out = 24u32.to_be_bytes().to_vec();
    out.extend_from_slice(b"ftypM4A ");
    out.extend_from_slice(&[0u8; 12]);
    out.extend_from_slice(&((payload + 8) as u32).to_be_bytes());
    out.extend_from_slice(b"mdat");
    out.extend(std::iter::repeat_n(0xAAu8, payload));
    out
}

/// One ISO-BMFF box: a big-endian total size, the type, then payload.
fn box_of(kind: &[u8; 4], total: usize) -> Vec<u8> {
    let mut out = (total as u32).to_be_bytes().to_vec();
    out.extend_from_slice(kind);
    out.extend(std::iter::repeat_n(0u8, total - 8));
    out
}

/// Bodies that only dress up as an audio container, each a file another program would act on or
/// a download cut short. Not a seal: a box sized to hold a whole archive still passes.
#[test]
fn a_body_that_only_dresses_up_as_an_audio_container_is_refused() {
    // A zero-size box means "to end of file" and would swallow any payload.
    let mut zero_size_box = box_of(b"ftyp", 16);
    zero_size_box.extend_from_slice(&0u32.to_be_bytes());
    zero_size_box.extend_from_slice(b"mdat");
    zero_size_box.extend_from_slice(b"PK\x03\x04 an archive read from its tail");

    let mut desktop = b"#abcftyp\n".to_vec();
    desktop.extend_from_slice(b"[Desktop Entry]\nType=Application\nExec=rm -rf /\n");

    let mut xml = b"<!--ftyp-->".to_vec();
    xml.extend_from_slice(b"<?xml version=\"1.0\"?><plist><dict/></plist>");

    let mut truncated = box_of(b"ftyp", 16);
    truncated.extend(box_of(b"moov", 40));
    let mut cut = box_of(b"mdat", 512);
    cut.truncate(cut.len() - 100);
    truncated.extend(cut);

    let mut with_zip = mp4(64);
    with_zip.extend_from_slice(b"PK\x03\x04 appended archive");

    let mut overrunning = 9999u32.to_be_bytes().to_vec();
    overrunning.extend_from_slice(b"ftypM4A ");
    overrunning.extend_from_slice(&[0u8; 16]);

    let mut flac_header_only = b"fLaC".to_vec();
    flac_header_only.push(0x80);
    flac_header_only.extend_from_slice(&[0x00, 0x00, 0x22]);
    flac_header_only.extend_from_slice(&[0u8; 34]);

    let mut flac_lying_block = b"fLaC".to_vec();
    flac_lying_block.push(0x80);
    flac_lying_block.extend_from_slice(&[0xFF, 0xFF, 0xFF]);
    flac_lying_block.extend_from_slice(&[0u8; 34]);

    let mut flac_not_streaminfo = b"fLaC".to_vec();
    flac_not_streaminfo.push(0x84); // last block, type 4 = VORBIS_COMMENT
    flac_not_streaminfo.extend_from_slice(&[0x00, 0x00, 0x22]);
    flac_not_streaminfo.extend_from_slice(&[0u8; 40]);

    // `ftyp` sits at offset 4, after the box size: carried anywhere else it is not ISO-BMFF.
    let mut ftyp_first = b"ftyp".to_vec();
    ftyp_first.extend_from_slice(&[0u8; 34]);

    let mut ftyp_buried = vec![0u8; 40];
    ftyp_buried[20..24].copy_from_slice(b"ftyp");

    // Covers the body exactly but does not open on `ftyp`: only the magic's own check refuses it.
    let mut unopened = box_of(b"moov", 16);
    unopened.extend(box_of(b"mdat", 32));

    for (body, what) in [
        (zero_size_box, "a zero-size box swallowing a payload"),
        (desktop, "desktop entry"),
        (xml, "xml/plist"),
        (truncated, "truncated DASH body"),
        (with_zip, "data appended after the chain"),
        (overrunning, "a box running past the end"),
        (flac_header_only, "no frames"),
        (flac_lying_block, "block runs past the end"),
        (flac_not_streaminfo, "a chain opening on VORBIS_COMMENT"),
        (
            b"[Desktop Entry]\nExec=rm -rf /\n".to_vec(),
            "a desktop entry",
        ),
        (b"#!/bin/sh\nrm -rf /\n".to_vec(), "a shell script"),
        (
            b"MZ\x90\x00\x03\x00\x00\x00\x04\x00".to_vec(),
            "a PE header",
        ),
        (b"<?xml version=\"1.0\"?><plist/>".to_vec(), "a bare plist"),
        (b"\x7fELF\x02\x01\x01\x00\x00\x00".to_vec(), "an ELF header"),
        (ftyp_first, "ftyp at offset 0"),
        (ftyp_buried, "ftyp deeper in the body"),
        (unopened, "a chain with no ftyp first"),
        (Vec::new(), "an empty body"),
        (b"fLa".to_vec(), "a truncated FLAC magic"),
        (b"fLaC".to_vec(), "a FLAC magic and nothing else"),
    ] {
        assert!(!is_audio_container(&body), "{what}");
    }
}
