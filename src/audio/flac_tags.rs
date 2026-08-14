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
        // Every header is rebuilt from the block's new position: a STREAMINFO that
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
#[path = "../../tests/unit/audio/flac_tags.rs"]
mod tests;
