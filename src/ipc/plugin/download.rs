//! `plugin.download`: the disk half of `MediaItem.download()`.
//!
//! The renderer computes the manifest, the destination and the tags but cannot write to
//! disk, so it hands them here: fetch the urls, decrypt with the player's decryptor, tag,
//! write. Decryption follows the manifest's `encryptionType`; tagging does not follow its
//! `codecs` field, see `run`.

use super::{IpcCallback, ipc_callback_err, ipc_callback_ok};
use crate::app_state::IpcMessage;
use crate::audio::flac_tags::{self, Picture};
use futures_util::StreamExt;
use serde::Deserialize;
use std::time::Duration;

/// TIDAL's own container. A DASH manifest needs an fMP4 remux that this does not do.
const BTS_MIME: &str = "application/vnd.tidal.bts";

/// Ceiling on one response body, enforced while streaming. The urls arrive as
/// plugin-supplied JSON, so an unbounded body would otherwise be fully buffered before
/// any check could fire. A hi-res track is tens of megabytes.
const MAX_BODY_BYTES: usize = 512 * 1024 * 1024;
/// Cover art is an image, not an album.
const MAX_COVER_BYTES: usize = 16 * 1024 * 1024;

/// Bounds a hang, not throughput: a peer that connects and then sends nothing would leave
/// the renderer's promise pending for the life of the process.
const TRACK_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const COVER_TIMEOUT: Duration = Duration::from_secs(30);

/// One download at a time, as upstream does: each holds a whole track in memory and the
/// retagger allocates a second copy, so a parallel album would multiply that into gigabytes.
static DOWNLOADS: std::sync::LazyLock<tokio::sync::Semaphore> =
    std::sync::LazyLock::new(|| tokio::sync::Semaphore::new(1));

/// Characters no filename may carry on Windows, plus the separators that would silently
/// turn a track title into a directory.
const ILLEGAL_NAME_CHARS: &[char] = &['/', '\\', ':', '*', '?', '"', '<', '>', '|'];

/// Longest filename component ext4, APFS and NTFS accept. A byte bound also satisfies NTFS'
/// limit of 255 UTF-16 units, since no UTF-8 byte expands to more than one unit.
const MAX_NAME_BYTES: usize = 255;

/// Windows refuses these as a filename stem whatever the extension.
const RESERVED_STEMS: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

#[derive(Deserialize)]
struct Request {
    #[serde(rename = "manifestMimeType")]
    manifest_mime_type: String,
    #[serde(default)]
    urls: Vec<String>,
    #[serde(rename = "keyId", default)]
    key_id: String,
    #[serde(rename = "encryptionType", default)]
    encryption_type: String,
    path: String,
    #[serde(default)]
    tags: Option<serde_json::Value>,
}

pub(super) fn handle_download(msg: &IpcMessage, callback: IpcCallback) {
    let payload = msg.arg(0).to_string();
    crate::state::rt_handle().spawn(async move {
        let req: Request = match serde_json::from_str(&payload) {
            Ok(req) => req,
            Err(e) => {
                ipc_callback_err(&callback, 400, &format!("bad download request: {e}"));
                return;
            }
        };
        if req.manifest_mime_type != BTS_MIME {
            ipc_callback_err(
                &callback,
                400,
                &format!(
                    "only {BTS_MIME} downloads are supported, got {}",
                    req.manifest_mime_type
                ),
            );
            return;
        }
        if req.urls.is_empty() {
            ipc_callback_err(&callback, 400, "manifest has no urls");
            return;
        }
        // Taken after validation so a bad request answers at once instead of queueing.
        let _permit = match DOWNLOADS.acquire().await {
            Ok(permit) => permit,
            Err(e) => {
                ipc_callback_err(&callback, 500, &format!("download queue closed: {e}"));
                return;
            }
        };
        match run(req).await {
            Ok(()) => ipc_callback_ok(&callback, "true"),
            Err(e) => ipc_callback_err(&callback, 500, &format!("{e:#}")),
        }
    });
}

async fn run(req: Request) -> anyhow::Result<()> {
    let dest = sanitized_destination(&req.path)?;
    // Upstream returns without doing anything when the file is already there, which is
    // what lets a plugin re-run over a library it has already downloaded.
    if dest.exists() {
        crate::vprintln!("[DOWNLOAD] Already present: {}", dest.display());
        return Ok(());
    }

    // Guessing from the key is what this replaces: a NONE manifest that still carries a
    // keyId would otherwise be run through the keystream and written as noise.
    let decrypt = match req.encryption_type.as_str() {
        "OLD_AES" => true,
        "NONE" => false,
        "" if req.key_id.is_empty() => false,
        other => anyhow::bail!("unusable manifest encryption type {other:?}"),
    };
    let data = fetch_track(&req.urls).await?;

    let key_id = req.key_id;
    let decrypted = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<u8>> {
        let mut data = data;
        if decrypt {
            crate::audio::decrypt::FlacDecryptor::new(&key_id)
                .and_then(|dec| dec.decrypt_in_place(&mut data, 0))?;
        }
        Ok(data)
    })
    .await??;

    // `codecs` lies in both directions: it reports non-FLAC for real FLAC streams (why
    // upstream dropped the check) and BTS does carry AAC, so the container decides. A
    // non-FLAC stream is written through untagged rather than failing the download.
    let finished = if req.tags.is_some() && decrypted.starts_with(b"fLaC") {
        let (comments, picture) = read_tags(req.tags.as_ref()).await;
        tokio::task::spawn_blocking(move || flac_tags::retag(decrypted, &comments, picture))
            .await??
    } else {
        decrypted
    };

    let size = finished.len() as u64;
    let shown = dest.display().to_string();
    write_file(dest, finished).await?;
    crate::vprintln!(
        "[DOWNLOAD] Wrote {shown} ({})",
        crate::util::fmt::format_bytes(size)
    );
    Ok(())
}

/// Sanitize the file name only, leaving the directory as the caller gave it, as upstream
/// does. The name is what carries a track title: an unsanitized `/` turns one into a
/// directory, and a `:` on Windows addresses an NTFS alternate data stream.
fn sanitized_destination(path: &str) -> anyhow::Result<std::path::PathBuf> {
    let raw = std::path::Path::new(path);
    let dir = raw
        .parent()
        .ok_or_else(|| anyhow::anyhow!("destination has no directory: {path}"))?;
    let name = raw
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow::anyhow!("destination has no file name: {path}"))?;

    let mut clean: String = name
        .chars()
        .map(|c| {
            if ILLEGAL_NAME_CHARS.contains(&c) || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect();
    // Windows silently strips a trailing dot or space, which would leave the written file
    // under a different name than the caller was told.
    while clean.ends_with('.') || clean.ends_with(' ') {
        clean.pop();
    }
    let stem = clean.split('.').next().unwrap_or("").to_uppercase();
    if RESERVED_STEMS.contains(&stem.as_str()) {
        clean.insert(0, '_');
    }
    clean = bound_name(&clean);
    // Cutting the stem can expose a trailing dot or space, which Windows strips again.
    while clean.ends_with('.') || clean.ends_with(' ') {
        clean.pop();
    }
    if clean.is_empty() {
        anyhow::bail!("destination file name is empty after sanitizing: {path}");
    }
    Ok(dir.join(clean))
}

/// Cut the stem, keep the extension. Bounded before anything is fetched: length is the same
/// class of refusal as an illegal character, and a late rename failure wastes a whole track.
fn bound_name(name: &str) -> String {
    if name.len() <= MAX_NAME_BYTES {
        return name.to_string();
    }
    // A leading dot names the file, it does not introduce an extension.
    let (stem, ext) = match name.rfind('.') {
        Some(at) if at > 0 => name.split_at(at),
        _ => (name, ""),
    };
    match MAX_NAME_BYTES
        .checked_sub(ext.len())
        .filter(|budget| *budget > 0)
    {
        Some(budget) => format!("{}{ext}", crate::util::truncate_str(stem, budget)),
        // An extension that fills the budget on its own leaves no stem to keep.
        None => crate::util::truncate_str(name, MAX_NAME_BYTES).to_string(),
    }
}

/// Stream and stop at the ceiling: `Content-Length` is absent on a chunked response, and a
/// buffered body is already allocated by the time it could be measured.
async fn fetch_track(urls: &[String]) -> anyhow::Result<Vec<u8>> {
    let mut out = Vec::new();
    for url in urls {
        let resp = crate::state::HTTP_CLIENT
            .get(url)
            .timeout(TRACK_TIMEOUT)
            .send()
            .await?;
        if !resp.status().is_success() {
            anyhow::bail!("upstream status {}", resp.status());
        }
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            if out.len() + chunk.len() > MAX_BODY_BYTES {
                anyhow::bail!("download exceeded the ceiling of {MAX_BODY_BYTES} bytes");
            }
            out.extend_from_slice(&chunk);
        }
    }
    Ok(out)
}

/// Write to a temp file in the destination directory, then rename, so an interrupted write
/// cannot leave a truncated file that `run`'s already-present check would skip for good.
/// Flushed before the rename to survive power loss; mode widened from the temp file's
/// owner-only default so a media server can read the library.
async fn write_file(dest: std::path::PathBuf, data: Vec<u8>) -> anyhow::Result<()> {
    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        use std::io::Write;

        let dir = dest
            .parent()
            .ok_or_else(|| anyhow::anyhow!("destination has no directory: {}", dest.display()))?;
        std::fs::create_dir_all(dir)?;

        let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
        tmp.write_all(&data)?;
        tmp.as_file().sync_all()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o644))?;
        }
        match tmp.persist_noclobber(&dest) {
            Ok(_) => Ok(()),
            // Skipping an existing file is the stated behaviour, so one that appeared
            // mid-download is the same outcome, not a failure; plain `persist` would replace it.
            Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => {
                crate::vprintln!(
                    "[DOWNLOAD] Destination appeared during the download, keeping it: {}",
                    dest.display()
                );
                Ok(())
            }
            Err(e) => Err(e.into()),
        }
    })
    .await?
}

/// A Vorbis field name is printable ASCII without `=`. The names arrive as JSON keys, so a
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
async fn read_tags(meta: Option<&serde_json::Value>) -> (Vec<(String, String)>, Option<Picture>) {
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

async fn fetch_cover(url: &str) -> Option<Picture> {
    let resp = match crate::state::HTTP_CLIENT
        .get(url)
        .timeout(COVER_TIMEOUT)
        .send()
        .await
    {
        Ok(resp) => resp,
        Err(e) => {
            crate::vprintln!("[DOWNLOAD] Cover fetch failed: {e}");
            return None;
        }
    };
    if !resp.status().is_success() {
        crate::vprintln!("[DOWNLOAD] Cover fetch returned {}", resp.status());
        return None;
    }

    // The spec requires printable ASCII here, so a parameterised or non-ASCII header
    // value is rejected rather than written into the block.
    let mime = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.split(';').next().unwrap_or(value).trim().to_string())
        .filter(|mime| mime.starts_with("image/") && mime.is_ascii())
        .unwrap_or_else(|| "image/jpeg".to_string());

    let mut data = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(e) => {
                crate::vprintln!("[DOWNLOAD] Cover body read failed: {e}");
                return None;
            }
        };
        if data.len() + chunk.len() > MAX_COVER_BYTES {
            crate::vprintln!("[DOWNLOAD] Cover art is over the size ceiling, skipping it");
            return None;
        }
        data.extend_from_slice(&chunk);
    }
    Some(Picture { mime, data })
}

#[cfg(test)]
#[path = "../../../tests/unit/ipc/plugin/download.rs"]
mod tests;
