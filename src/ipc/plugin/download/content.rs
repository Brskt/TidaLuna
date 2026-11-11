use super::net::fetch_cover;
use crate::audio::flac_tags::Picture;

/// Checks structure (a FLAC block chain or an ISO-BMFF box chain accounting for every byte), not a
/// magic prefix, which `#abcftyp\n[Desktop Entry]` fools. Judged on bytes, never the file name:
/// `fileExtension()` answers `flac` for every BTS manifest including AAC, which arrives as ISO-BMFF.
///
/// This only proves the file cannot ALSO be parsed as something else from offset 0. It seals neither
/// the opaque payload (`mdat`, FLAC frames) nor the plugin's tags added after it: an archive reader
/// finds a ZIP in those. What keeps such a reader away is the audio name `run` holds the file to.
pub(super) fn is_audio_container(data: &[u8]) -> bool {
    is_flac_stream(data) || is_iso_bmff(data)
}

/// A FLAC signature at offset 0 plus a metadata block chain that terminates inside the data. Nothing
/// may precede the signature, which is what a prefix polyglot cannot satisfy.
fn is_flac_stream(data: &[u8]) -> bool {
    if !data.starts_with(b"fLaC") {
        return false;
    }
    let mut at = 4;
    loop {
        // 1 byte of last-block flag plus block type, then a 24-bit big-endian length.
        let header = match data.get(at..at + 4) {
            Some(h) => h,
            None => return false,
        };
        // STREAMINFO must open the chain, and it is the one block with a fixed length.
        if at == 4
            && (header[0] & 0x7F != 0
                || u32::from_be_bytes([0, header[1], header[2], header[3]]) != 34)
        {
            return false;
        }
        let len = u32::from_be_bytes([0, header[1], header[2], header[3]]) as usize;
        at = match at.checked_add(4).and_then(|a| a.checked_add(len)) {
            Some(a) if a <= data.len() => a,
            // A block that runs past the end describes something this is not.
            _ => return false,
        };
        if header[0] & 0x80 != 0 {
            // Last block. Frames follow, and there must be some: a header alone is not a track.
            return at < data.len();
        }
    }
}

/// An ISO-BMFF box chain opening on `ftyp` and accounting for every byte to the end. The accounting
/// rejects both a polyglot prefix and appended data: the first four bytes are a size; text there
/// yields one that does not land on the end.
fn is_iso_bmff(data: &[u8]) -> bool {
    let mut at = 0usize;
    let mut first = true;
    while at < data.len() {
        let header = match data.get(at..at + 8) {
            Some(h) => h,
            None => return false,
        };
        if first && &header[4..8] != b"ftyp" {
            return false;
        }
        first = false;
        let size = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as usize;
        let size = match size {
            // Spec-legal for a final box, meaning "to end of file", which is the escape hatch: `ftyp`
            // then a zero-size box swallows any payload and the walk still terminates happily. TIDAL
            // writes explicit sizes.
            0 => return false,
            // 1 means a 64-bit size follows the type. Accepted for real media, whose `mdat` needs
            // it past 4 GiB.
            1 => {
                let ext = match data.get(at + 8..at + 16) {
                    Some(e) => e,
                    None => return false,
                };
                match usize::try_from(u64::from_be_bytes(ext.try_into().unwrap_or([0; 8]))) {
                    Ok(s) if s >= 16 => s,
                    _ => return false,
                }
            }
            s if s >= 8 => s,
            // Smaller than its own header.
            _ => return false,
        };
        at = match at.checked_add(size) {
            Some(a) if a <= data.len() => a,
            // A chain that runs past the end does not describe this file.
            _ => return false,
        };
    }
    // Landed exactly on the end, and saw at least the `ftyp`.
    !first
}

/// A Vorbis field name is printable ASCII without `=`. The names arrive as JSON keys: a
/// crafted one could emit an entry parsers split in the wrong place and read as a value.
fn valid_field_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| (0x20..=0x7D).contains(&b) && b != b'=')
}

/// The tag keys are Vorbis field names once upper-cased, with one exception: `genres`
/// would become `GENRES`, and the field every player reads is the singular `GENRE`.
fn vorbis_field(key: &str) -> String {
    if key.eq_ignore_ascii_case("genres") {
        return "GENRE".to_string();
    }
    key.to_uppercase()
}

/// A cover that cannot be fetched is logged and skipped: the track is still worth writing.
pub(super) async fn read_tags(
    meta: Option<&serde_json::Value>,
) -> (Vec<(String, String)>, Option<Picture>) {
    let Some(meta) = meta else {
        return (Vec::new(), None);
    };

    let mut comments = Vec::new();
    if let Some(fields) = meta.get("tags").and_then(|t| t.as_object()) {
        for (key, value) in fields {
            let field = vorbis_field(key);
            if !valid_field_name(&field) {
                crate::vprintln!("[DOWNLOAD] Skipping tag with an unusable field name {key:?}");
                continue;
            }
            match value {
                serde_json::Value::String(text) if !text.is_empty() => {
                    comments.push((field, text.clone()));
                }
                // Vorbis repeats a field for multiple values rather than joining them.
                serde_json::Value::Array(items) => {
                    for item in items.iter().filter_map(|i| i.as_str()) {
                        if !item.is_empty() {
                            comments.push((field.clone(), item.to_string()));
                        }
                    }
                }
                serde_json::Value::Null => {}
                other => {
                    crate::vprintln2!("[DOWNLOAD] Skipping tag {field} of unusable shape: {other}")
                }
            }
        }
    }

    let picture = match meta.get("coverUrl").and_then(|u| u.as_str()) {
        Some(url) if !url.is_empty() => fetch_cover(url).await,
        _ => None,
    };
    (comments, picture)
}

#[cfg(test)]
#[path = "../../../../tests/unit/ipc/plugin/download/content.rs"]
mod tests;
