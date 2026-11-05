//! Checking a cover the page transcoded before it is cached and handed to Chromium's media stack.
//!
//! Untrusted: any script in the page can post a file. Checked against what the preceding decode
//! knows (size, duration) and what `video/` claims to serve (a WebM of one VP9 track); Chromium's
//! demuxer parses it again at play, and this check only gates the cache.

use symphonia::core::codecs::CodecParameters;
use symphonia::core::codecs::video::well_known::CODEC_ID_VP9;
use symphonia::core::formats::{FormatOptions, FormatReader};
use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions};
use symphonia::default::formats::MkvReader;

/// Well above what the page's 4 Mbps target writes for a cover. The ceiling bounds what the
/// handler reads into memory from a body whose size the page declares.
pub(crate) const MAX_WEBM_BYTES: usize = 8 * 1024 * 1024;

/// What the decode that preceded the upload says the WebM must be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ExpectedCover {
    pub(crate) width: usize,
    pub(crate) height: usize,
    pub(crate) duration_micros: i64,
    pub(crate) tolerance_micros: i64,
}

/// The EBML header's ID, which is also the file's first four bytes.
const EBML_ID: u32 = 0x1A45_DFA3;

const DOC_TYPE_ID: u32 = 0x4282;

/// An element ID at `at`, marker bits kept as the specification writes IDs, and its length.
fn element_id(bytes: &[u8], at: usize) -> Option<(u32, usize)> {
    let first = *bytes.get(at)?;
    let len = first.leading_zeros() as usize + 1;
    if len > 4 {
        return None;
    }
    let raw = bytes.get(at..at + len)?;
    Some((raw.iter().fold(0, |id, &b| id << 8 | u32::from(b)), len))
}

/// An element size at `at`, marker bit removed, and its length. The all-ones "unknown size" comes
/// out as a value no buffer holds, which the caller's bounds check then refuses.
fn element_size(bytes: &[u8], at: usize) -> Option<(u64, usize)> {
    let first = *bytes.get(at)?;
    let len = first.leading_zeros() as usize + 1;
    if len > 8 {
        return None;
    }
    let rest = bytes.get(at + 1..at + len)?;
    let value = rest.iter().fold(u64::from(first) & (0xFF >> len), |v, &b| {
        v << 8 | u64::from(b)
    });
    Some((value, len))
}

/// The `DocType` of the EBML header that opens `bytes`. Read here because the Matroska reader
/// accepts `matroska` as well as `webm` and does not say which it found.
fn doc_type(bytes: &[u8]) -> Option<&str> {
    let (id, id_len) = element_id(bytes, 0)?;
    if id != EBML_ID {
        return None;
    }
    let (size, size_len) = element_size(bytes, id_len)?;
    let start = id_len + size_len;
    let header = bytes.get(start..start.checked_add(usize::try_from(size).ok()?)?)?;

    let mut at = 0;
    while at < header.len() {
        let (id, id_len) = element_id(header, at)?;
        let (size, size_len) = element_size(header, at + id_len)?;
        let body_start = at + id_len + size_len;
        let body_end = body_start.checked_add(usize::try_from(size).ok()?)?;
        let body = header.get(body_start..body_end)?;
        if id == DOC_TYPE_ID {
            return std::str::from_utf8(body)
                .ok()
                .map(|s| s.trim_end_matches('\0'));
        }
        at = body_end;
    }
    None
}

/// Accept `bytes` as the cover the page was asked to transcode, or say why not.
pub(crate) fn validate_cover_webm(bytes: &[u8], expected: &ExpectedCover) -> Result<(), String> {
    if doc_type(bytes) != Some("webm") {
        return Err("the upload is not a WebM file".to_string());
    }

    let source = MediaSourceStream::new(
        Box::new(std::io::Cursor::new(bytes.to_vec())),
        MediaSourceStreamOptions::default(),
    );
    let reader = MkvReader::try_new(source, FormatOptions::default())
        .map_err(|e| format!("the upload is not a readable WebM file: {e}"))?;

    let [track] = reader.tracks() else {
        return Err("the upload does not hold exactly one track".to_string());
    };
    let Some(CodecParameters::Video(video)) = track.codec_params.as_ref() else {
        return Err("the upload's track is not video".to_string());
    };
    if video.codec != CODEC_ID_VP9 {
        return Err("the upload's video is not VP9".to_string());
    }
    let size = (video.width.map(usize::from), video.height.map(usize::from));
    if size != (Some(expected.width), Some(expected.height)) {
        return Err(format!(
            "the upload is {:?}x{:?}, the cover {}x{}",
            size.0, size.1, expected.width, expected.height
        ));
    }

    let media = reader.media_info();
    let (Some(duration), Some(time_base)) = (media.duration, media.time_base) else {
        return Err("the upload states no duration".to_string());
    };
    let micros = u128::from(duration.get()) * u128::from(time_base.numer.get()) * 1_000_000
        / u128::from(time_base.denom.get());
    let micros =
        i64::try_from(micros).map_err(|_| "the upload's duration overflows".to_string())?;
    if (micros - expected.duration_micros).abs() > expected.tolerance_micros {
        return Err(format!(
            "the upload lasts {micros} us, the cover {} us",
            expected.duration_micros
        ));
    }
    Ok(())
}

#[cfg(test)]
#[path = "../../tests/unit/video/webm.rs"]
mod tests;
