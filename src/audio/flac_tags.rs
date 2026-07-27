//! Write Vorbis comments and cover art into a FLAC stream.
//!
//! Hand-rolled rather than a dependency: the work is write-only and the block layout is
//! stable.

/// Cover art for the PICTURE block. `mime` must be printable ASCII per the spec.
pub(crate) struct Picture {
    pub(crate) mime: String,
    pub(crate) data: Vec<u8>,
}

const VORBIS_COMMENT: u8 = 4;
const PICTURE: u8 = 6;
/// Picture type 3 in the FLAC spec's table.
const FRONT_COVER: u32 = 3;
/// A metadata block's length field is 24 bits.
const MAX_BLOCK: usize = 0x00FF_FFFF;

/// Replace the comment and picture blocks, keeping every other block and the audio frames.
/// Replacing rather than appending: a stream tagged twice would otherwise carry duplicate
/// blocks and grow its header on every pass.
pub(crate) fn retag(
    stream: Vec<u8>,
    comments: &[(String, String)],
    picture: Option<Picture>,
) -> anyhow::Result<Vec<u8>> {
    if !stream.starts_with(b"fLaC") {
        anyhow::bail!("not a FLAC stream");
    }

    let mut kept: Vec<(u8, std::ops::Range<usize>)> = Vec::new();
    let mut cursor = 4usize;
    loop {
        let header = *stream
            .get(cursor)
            .ok_or_else(|| anyhow::anyhow!("FLAC metadata ends mid-header"))?;
        let len_bytes = stream
            .get(cursor + 1..cursor + 4)
            .ok_or_else(|| anyhow::anyhow!("FLAC metadata ends mid-header"))?;
        let len = u32::from_be_bytes([0, len_bytes[0], len_bytes[1], len_bytes[2]]) as usize;

        let body = cursor + 4;
        let end = body
            .checked_add(len)
            .filter(|e| *e <= stream.len())
            .ok_or_else(|| anyhow::anyhow!("FLAC block runs past the stream"))?;

        let block_type = header & 0x7F;
        if block_type != VORBIS_COMMENT && block_type != PICTURE {
            kept.push((block_type, body..end));
        }
        cursor = end;
        if header & 0x80 != 0 {
            break;
        }
    }
    let audio_start = cursor;

    let mut blocks: Vec<(u8, Vec<u8>)> = kept
        .into_iter()
        .map(|(block_type, body)| (block_type, stream[body].to_vec()))
        .collect();
    blocks.push((VORBIS_COMMENT, vorbis_comment_body(comments)));

    if let Some(pic) = picture {
        let body = picture_body(&pic);
        if body.len() <= MAX_BLOCK {
            blocks.push((PICTURE, body));
        } else {
            // The track matters more than its artwork.
            crate::vprintln!("[DOWNLOAD] Cover art exceeds a metadata block, skipping it");
        }
    }

    let mut out = Vec::with_capacity(stream.len() + 4096);
    out.extend_from_slice(b"fLaC");
    let last = blocks.len() - 1;
    for (index, (block_type, body)) in blocks.iter().enumerate() {
        if body.len() > MAX_BLOCK {
            anyhow::bail!("metadata block of {} bytes exceeds 24 bits", body.len());
        }
        // Every header is rebuilt from the block's new position, so a STREAMINFO that
        // was flagged last in the input loses the flag once a comment follows it.
        let flag = if index == last { 0x80 } else { 0x00 };
        out.push(flag | block_type);
        out.extend_from_slice(&(body.len() as u32).to_be_bytes()[1..]);
        out.extend_from_slice(body);
    }
    out.extend_from_slice(&stream[audio_start..]);
    Ok(out)
}

/// Vendor and entry lengths here are little endian, unlike every other length in FLAC:
/// the comment block keeps Vorbis' own byte order.
fn vorbis_comment_body(comments: &[(String, String)]) -> Vec<u8> {
    let vendor = concat!("TidaLunar ", env!("CARGO_PKG_VERSION"));
    let mut out = Vec::new();
    out.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
    out.extend_from_slice(vendor.as_bytes());
    out.extend_from_slice(&(comments.len() as u32).to_le_bytes());
    for (field, value) in comments {
        let entry = format!("{field}={value}");
        out.extend_from_slice(&(entry.len() as u32).to_le_bytes());
        out.extend_from_slice(entry.as_bytes());
    }
    out
}

/// Every length in a PICTURE block is big endian. Dimensions and colour count are left at
/// zero, which the spec reads as unspecified, sparing a decode just to restate the size.
fn picture_body(pic: &Picture) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&FRONT_COVER.to_be_bytes());
    out.extend_from_slice(&(pic.mime.len() as u32).to_be_bytes());
    out.extend_from_slice(pic.mime.as_bytes());
    out.extend_from_slice(&0u32.to_be_bytes()); // no description
    for _ in 0..4 {
        out.extend_from_slice(&0u32.to_be_bytes());
    }
    out.extend_from_slice(&(pic.data.len() as u32).to_be_bytes());
    out.extend_from_slice(&pic.data);
    out
}

#[cfg(test)]
mod tests {
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
}
