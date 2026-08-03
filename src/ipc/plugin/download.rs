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

/// TIDAL's own container: one url, FLAC or AAC, encrypted per the manifest.
const BTS_MIME: &str = "application/vnd.tidal.bts";
/// Segmented AAC. The init segment and its media segments concatenate into a fragmented MP4, which
/// plays as written; upstream additionally remuxes it to progressive MP4 and tags it, which this does
/// not do. The result is untagged.
const DASH_MIME: &str = "application/dash+xml";

/// Longest url list a real manifest produces. The list is plugin-supplied; without a bound, Rust
/// would issue as many requests as asked, and the byte ceiling does not stop that, since a list of
/// tiny 404s costs nothing per url. A captured DASH track ran 60 segments. This is headroom.
const MAX_URLS: usize = 4096;

fn is_supported_manifest(mime: &str) -> bool {
    mime == BTS_MIME || mime == DASH_MIME
}

fn url_count_within_bounds(count: usize) -> bool {
    count > 0 && count <= MAX_URLS
}

/// Bounds the whole accumulated buffer (all `MAX_URLS` segments share it), not one response.
/// Unbounded plugin-supplied urls would otherwise buffer fully before any check fires.
const MAX_DOWNLOAD_BYTES: usize = 512 * 1024 * 1024;
/// Cover art is an image, not an album.
const MAX_COVER_BYTES: usize = 16 * 1024 * 1024;

/// Bounds a hang, not throughput: a peer that connects and then sends nothing would leave
/// the renderer's promise pending for the life of the process.
const TRACK_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// Bounds the whole run, not one url. A 4096-url list each inside its own `TRACK_TIMEOUT` would
/// otherwise hold the single download permit for months.
const WHOLE_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(60 * 60);
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

/// Hosts TIDAL serves audio from. A label-boundary suffix, never a substring: `evil-audio.tidal.com`
/// is a host anyone can register and it contains the string.
fn is_tidal_media_host(host: &str) -> bool {
    host == "audio.tidal.com" || host.ends_with(".audio.tidal.com")
}

/// Cover art comes from its own host and does not belong on the media list.
fn is_tidal_artwork_host(host: &str) -> bool {
    host == "resources.tidal.com"
}

/// Host comes from the parsed url: `https://lgf.audio.tidal.com@evil.com/` reads as `evil.com` and is
/// refused; HTTPS only. General outbound http is the trust-gated native `http(s)` module. This must
/// not become a second, unprompted way to the same thing.
fn allowed_url(url: &str, allow: fn(&str) -> bool) -> bool {
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    parsed.scheme() == "https" && allow(parsed.host_str().unwrap_or(""))
}

/// The same rule at every redirect hop. Checking the submitted url alone would be bypassed by an
/// open redirect on an allowed host, and the shared client follows redirects with no policy at all.
/// Media and artwork share one client; either host is acceptable mid-chain.
fn redirect_allowed(url: &url::Url) -> bool {
    let host = url.host_str().unwrap_or("");
    url.scheme() == "https" && (is_tidal_media_host(host) || is_tidal_artwork_host(host))
}

/// Downloads use their own client. The redirect policy stays off every other caller of
/// `HTTP_CLIENT`.
static DOWNLOAD_CLIENT: std::sync::LazyLock<reqwest::Client> = std::sync::LazyLock::new(|| {
    crate::state::build_client_with_redirect(reqwest::redirect::Policy::custom(|attempt| {
        if !redirect_allowed(attempt.url()) {
            return attempt.error(Refused("redirect left the allowed TIDAL hosts"));
        }
        if attempt.previous().len() >= 10 {
            return attempt.stop();
        }
        attempt.follow()
    }))
});

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
        if !is_supported_manifest(&req.manifest_mime_type) {
            ipc_callback_err(
                &callback,
                400,
                &format!("unsupported manifest type {}", req.manifest_mime_type),
            );
            return;
        }
        if !url_count_within_bounds(req.urls.len()) {
            ipc_callback_err(
                &callback,
                400,
                &format!(
                    "manifest must carry 1 to {MAX_URLS} urls, got {}",
                    req.urls.len()
                ),
            );
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
            Err(e) => ipc_callback_err(&callback, status_for(&e), &format!("{e:#}")),
        }
    });
}

/// A refusal this channel answers 403 with. A type, not a string match; a future refusal gets the
/// right code without anyone remembering a list.
#[derive(Debug)]
struct Refused(&'static str);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for Refused {}

/// 403 for a permanent refusal, 500 for anything else. Walks the whole error chain: reqwest wraps the
/// redirect policy's `Refused` as a source, and checking only the top answered 500 for a refusal.
fn status_for(e: &anyhow::Error) -> i32 {
    if e.chain().any(|cause| cause.is::<Refused>()) {
        403
    } else {
        500
    }
}

/// Bounded host only, never the full url: it is plugin-supplied and `verr!` is not gated. Logging it
/// verbatim would let the renderer write unbounded text, escapes intact, into the persistent log.
/// The host is what makes an unexpected regional CDN reportable.
fn refused_host(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(parsed) => {
            crate::util::truncate_str(parsed.host_str().unwrap_or("no host"), 64).to_string()
        }
        Err(_) => "unparseable url".to_string(),
    }
}

async fn run(req: Request) -> anyhow::Result<()> {
    let dest = sanitized_destination(&req.path)?;
    // Upstream returns without doing anything when the file is already there, which is
    // what lets a plugin re-run over a library it has already downloaded.
    if dest.exists() {
        crate::vprintln!("[DOWNLOAD] Already present: {}", dest.display());
        return Ok(());
    }

    // DASH is never encrypted (no key fields; `Player::load_dash` decrypts nothing). Stated rather
    // than inferred: a manifest must not claim otherwise and run segments through the FLAC keystream.
    // BTS follows its own field: a NONE manifest still carrying a keyId would otherwise be decrypted.
    let decrypt = if req.manifest_mime_type == DASH_MIME {
        false
    } else {
        match req.encryption_type.as_str() {
            "OLD_AES" => true,
            "NONE" => false,
            "" if req.key_id.is_empty() => false,
            other => anyhow::bail!("unusable manifest encryption type {other:?}"),
        }
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

    // A plugin picks the dialog's `defaultPath`; a user accepting without reading it can be steered
    // at a file another program acts on. Refusing non-audio keeps this channel from writing one.
    if !is_audio_container(&decrypted) {
        return Err(Refused("the stream is not an audio container").into());
    }

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

/// Checks structure (a FLAC block chain or an ISO-BMFF box chain accounting for every byte), not a
/// magic prefix, which `#abcftyp\n[Desktop Entry]` fools. Judged on bytes, never the file name:
/// `fileExtension()` answers `flac` for every BTS manifest including AAC, which arrives as ISO-BMFF.
///
/// This only proves the file cannot ALSO be parsed as something else from offset 0, the real risk of
/// a plugin-picked `defaultPath`. It does not seal the opaque payload region (`mdat`, FLAC frames)
/// against carried content; that needs a demuxer, not this.
fn is_audio_container(data: &[u8]) -> bool {
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

/// What is left of the run's deadline, capped at the per-request limit; `None` once it has passed.
/// Derived from the deadline rather than tracked beside it: reqwest's timeout covers the streamed
/// body; clamping it is what bounds the run. A request admitted just before the deadline with its
/// own full `TRACK_TIMEOUT` would otherwise hold the single download permit half again as long.
fn next_request_timeout(remaining: Duration) -> Option<Duration> {
    (!remaining.is_zero()).then(|| TRACK_TIMEOUT.min(remaining))
}

/// Stream and stop at the ceiling: `Content-Length` is absent on a chunked response, and a
/// buffered body is already allocated by the time it could be measured.
async fn fetch_track(urls: &[String]) -> anyhow::Result<Vec<u8>> {
    // `TRACK_TIMEOUT` bounds one url. With a list that long, a peer trickling a byte inside each
    // url's own window would hold the single download permit for months and block every other
    // plugin. The whole run gets a deadline too.
    let deadline = tokio::time::Instant::now() + WHOLE_DOWNLOAD_TIMEOUT;
    let mut out = Vec::new();
    for url in urls {
        if !allowed_url(url, is_tidal_media_host) {
            // Gated, like the ingress refusal in `super::mod`: the caller learns of it from its own
            // 403 (the log is not the only channel), and an ungated write would let a refused-call
            // loop drive the disk from the renderer.
            crate::vprintln!(
                "[DOWNLOAD] Refused a url outside the TIDAL media hosts: {}",
                refused_host(url)
            );
            return Err(Refused("download url is not a TIDAL media url").into());
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let Some(timeout) = next_request_timeout(remaining) else {
            anyhow::bail!("download exceeded {WHOLE_DOWNLOAD_TIMEOUT:?}");
        };
        let resp = DOWNLOAD_CLIENT.get(url).timeout(timeout).send().await?;
        if !resp.status().is_success() {
            anyhow::bail!("upstream status {}", resp.status());
        }
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            if out.len() + chunk.len() > MAX_DOWNLOAD_BYTES {
                anyhow::bail!("download exceeded the ceiling of {MAX_DOWNLOAD_BYTES} bytes");
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
    if !allowed_url(url, is_tidal_artwork_host) {
        crate::vprintln!("[DOWNLOAD] Cover url is not a TIDAL artwork url, skipping it");
        return None;
    }
    let resp = match DOWNLOAD_CLIENT.get(url).timeout(COVER_TIMEOUT).send().await {
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
