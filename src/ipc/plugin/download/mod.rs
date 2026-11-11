//! `plugin.download`: the disk half of `MediaItem.download()`.
//!
//! The renderer computes the manifest, the destination and the tags but cannot write to
//! disk; it hands them here: fetch the urls, decrypt with the player's decryptor, remux a DASH
//! track, tag, write. Decryption follows the manifest's `encryptionType`; tagging does not follow
//! its `codecs` field, see `run`.
//!
//! The renderer names the destination. A write is only legitimate if `ui::file_dialog`'s ledger shows
//! the user answered a save or folder dialog for it; otherwise this channel is a general file-write
//! primitive.

use super::{IpcCallback, ipc_callback_err, ipc_callback_ok};
use crate::app_state::IpcMessage;
use crate::audio::flac_tags;
use crate::audio::{fmp4_remux, mp4_tags};
use crate::util::Refused;
use serde::Deserialize;

mod content;
mod name;
mod net;

use content::{is_audio_container, read_tags};
pub(super) use name::{audio_named, sanitized_destination};
use net::fetch_track;

/// TIDAL's own container: one url, FLAC or AAC, encrypted per the manifest.
const BTS_MIME: &str = "application/vnd.tidal.bts";
/// Segmented AAC. The init segment and its media segments concatenate into a fragmented MP4, which
/// is remuxed to a progressive MP4, tagged when tags are sent.
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

/// One download at a time, as upstream does: each holds a whole track in memory and the
/// retagger allocates a second copy. A parallel album would multiply that into gigabytes.
static DOWNLOADS: std::sync::LazyLock<tokio::sync::Semaphore> =
    std::sync::LazyLock::new(|| tokio::sync::Semaphore::new(1));

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
        // Taken after validation: a bad request answers at once instead of queueing.
        let _permit = match DOWNLOADS.acquire().await {
            Ok(permit) => permit,
            Err(e) => {
                ipc_callback_err(&callback, 500, &format!("download queue closed: {e}"));
                return;
            }
        };
        match run(req, crate::platform::install_guard::get()).await {
            Ok(()) => ipc_callback_ok(&callback, "true"),
            Err(e) => ipc_callback_err(&callback, status_for(&e), &format!("{e:#}")),
        }
    });
}

/// 403 for a permanent refusal, 500 for anything else. Walks the whole error chain: reqwest wraps the
/// redirect policy's `Refused` as a source, and checking only the top answered 500 for a refusal.
fn status_for(e: &anyhow::Error) -> i32 {
    if e.chain().any(|cause| cause.is::<Refused>()) {
        403
    } else {
        500
    }
}

/// What a plugin hears when a download targets the app's own files.
const IN_THE_INSTALL: &str = "destination is inside TidaLunar's own files";

/// What a plugin hears when a folder grant reaches the home folder under another path.
const INTO_THE_HOME: &str = "destination is inside the home folder, reached by another path";

/// What a plugin hears when it names a download for something other than audio.
const NOT_AUDIO_NAME: &str = "destination must end in .flac or .m4a";

async fn run(
    req: Request,
    guard: Option<&'static crate::platform::install_guard::Protected>,
) -> anyhow::Result<()> {
    let dest = sanitized_destination(&req.path)?;
    // Auth and identity are matched, not spent; a failed fetch must not cost the user their answer.
    // A symlink swap of an ancestor between here and the rename is an accepted window: closing it
    // needs a directory handle `tempfile` cannot use, and plugins have no symlink call.
    // Canonicalising ancestors blocks. This runs off the async worker rather than pinning a runtime
    // thread on a stalled network mount.
    // The snapshot is taken after the download permit: an already-finished sibling's write must be
    // visible here, and anchoring at dialog time would flag our own queued write as tampering.
    let (auth, enters_home, before, protected) = {
        let dest = dest.clone();
        tokio::task::spawn_blocking(move || {
            use crate::ui::file_dialog::{
                Authorisation, authorisation_for, folder_write_enters_home,
            };
            let auth = authorisation_for(&dest);
            let enters_home =
                matches!(auth, Some(Authorisation::Folder)) && folder_write_enters_home(&dest);
            (
                auth,
                enters_home,
                FileIdentity::of(&dest),
                guard.is_some_and(|g| g.covers(&dest)),
            )
        })
        .await?
    };
    let auth = auth.ok_or(Refused("destination was not chosen in a save dialog"))?;
    // Judged before the fetch and before the existing-file check: an install file answers alike
    // present or absent. `write_file` judges again at the rename.
    if protected {
        return Err(Refused(IN_THE_INSTALL).into());
    }
    if enters_home {
        return Err(Refused(INTO_THE_HOME).into());
    }
    // The plugin names the file, with no dialog at all under a folder grant, and its tags join the
    // bytes after the content check below: only the name can keep another program from opening it.
    if !audio_named(&dest) {
        return Err(Refused(NOT_AUDIO_NAME).into());
    }

    // Settled before anything is fetched, from the same policy the write applies: the two cannot
    // disagree. Asking here rather than at the rename is what keeps a doomed download from pulling
    // and decrypting a whole track first.
    if before.is_some() {
        match OnExisting::for_authorisation(&auth) {
            // Nobody was asked about this file; it is left alone and the track counts as done.
            // Upstream behaved this way too, which is what lets a re-run over a part-downloaded
            // album finish instead of failing track by track.
            OnExisting::Skip => {
                crate::vprintln!("[DOWNLOAD] Already present, keeping it: {}", dest.display());
                return Ok(());
            }
            // The OS prompts only for the name it was handed: a sanitised rename means the
            // answer confirmed nothing about the path actually being written.
            OnExisting::Fail => {
                return Err(Refused("destination exists and no replacement was confirmed").into());
            }
            OnExisting::Replace => {}
        }
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

    // The name is audio already; the bytes must be too, from offset 0, where a reader that sniffs
    // content looks first.
    if !is_audio_container(&decrypted) {
        return Err(Refused("the stream is not an audio container").into());
    }

    // `codecs` lies in both directions: it reports non-FLAC for real FLAC streams (why
    // upstream dropped the check) and BTS does carry AAC; the container decides. A
    // non-FLAC BTS stream is written through untagged rather than failing the download.
    let finished = if req.manifest_mime_type == DASH_MIME {
        // A remux that fails fails the download: the fragments are not
        // the music file the plugin asked for.
        let tags = match req.tags.as_ref() {
            Some(meta) => {
                let (comments, picture) = read_tags(Some(meta)).await;
                Some(mp4_tags::udta_body(&comments, picture.as_ref())?)
            }
            None => None,
        };
        tokio::task::spawn_blocking(move || fmp4_remux::to_progressive(&decrypted, tags)).await??
    } else if req.tags.is_some() && decrypted.starts_with(b"fLaC") {
        let (comments, picture) = read_tags(req.tags.as_ref()).await;
        tokio::task::spawn_blocking(move || flac_tags::retag(decrypted, &comments, picture))
            .await??
    } else {
        decrypted
    };

    let size = finished.len() as u64;
    let shown = dest.display().to_string();
    let outcome = write_file(
        dest,
        finished,
        OnExisting::for_authorisation(&auth),
        before,
        guard,
    )
    .await?;
    // Spent only now: one save dialog answer buys one file on disk, and a failure above leaves the
    // answer usable for the retry. A folder grant is not spent at all.
    crate::ui::file_dialog::consume(auth);
    match outcome {
        WriteOutcome::Written => crate::vprintln!(
            "[DOWNLOAD] Wrote {shown} ({})",
            crate::util::fmt::format_bytes(size)
        ),
        WriteOutcome::Skipped => {
            crate::vprintln!("[DOWNLOAD] Appeared during the download, keeping it: {shown}")
        }
    }
    Ok(())
}

/// Enough to tell "still the file this run was authorised against" from "something else now": consent
/// names a file, and the fetch can run an hour before the rename. Unix uses `(dev, ino)`; Windows has
/// no stable equivalent (rust#63010); timestamps are a signal there, not proof.
#[derive(PartialEq, Eq)]
struct FileIdentity {
    len: u64,
    marks: (u64, u64),
}

impl FileIdentity {
    /// `None` when nothing is there, which the comparison must tell apart from a file: "was absent,
    /// is absent" is a match, "was absent, something arrived" is not.
    fn of(path: &std::path::Path) -> Option<Self> {
        let meta = std::fs::metadata(path).ok()?;
        #[cfg(unix)]
        let marks = {
            use std::os::unix::fs::MetadataExt;
            (meta.dev(), meta.ino())
        };
        #[cfg(windows)]
        let marks = {
            use std::os::windows::fs::MetadataExt;
            (meta.creation_time(), meta.last_write_time())
        };
        #[cfg(not(any(unix, windows)))]
        let marks = (0, 0);
        Some(Self {
            len: meta.len(),
            marks,
        })
    }
}

/// What to do when the destination exists at the rename. One value rather than a bool plus a check
/// elsewhere: the pre-fetch test and the rename must not disagree, and they did: a file that appeared
/// mid-download failed the track with `AlreadyExists` while the pre-fetch check called it done.
#[derive(Clone, Copy)]
enum OnExisting {
    /// The user answered a replace prompt for this exact name.
    Replace,
    /// A folder grant: nobody was asked about this file; it is kept and the track counts as done.
    Skip,
    /// A save dialog answer that did not confirm a replacement. Refuse rather than destroy.
    Fail,
}

impl OnExisting {
    /// Derived from the authorisation: the policy has one origin.
    fn for_authorisation(auth: &crate::ui::file_dialog::Authorisation) -> Self {
        if auth.replace_confirmed() {
            Self::Replace
        } else if auth.skips_existing() {
            Self::Skip
        } else {
            Self::Fail
        }
    }
}

/// Whether anything was actually written. The caller's log must not claim a write that was
/// skipped.
enum WriteOutcome {
    Written,
    Skipped,
}

/// Write to a temp file in the destination directory, then rename: an interrupted write
/// leaves the previous file in place rather than a truncated one. Flushed before the rename to
/// survive power loss; mode widened from the temp file's owner-only default for a media
/// server to read the library.
async fn write_file(
    dest: std::path::PathBuf,
    data: Vec<u8>,
    on_existing: OnExisting,
    before: Option<FileIdentity>,
    guard: Option<&'static crate::platform::install_guard::Protected>,
) -> anyhow::Result<WriteOutcome> {
    tokio::task::spawn_blocking(move || -> anyhow::Result<WriteOutcome> {
        use std::io::Write;

        // The app's own files are never a download's to touch, whatever the dialog answered.
        // Judged before a directory is created, then again right before the rename.
        let protected = |path: &std::path::Path| guard.is_some_and(|g| g.covers(path));
        let dir = dest
            .parent()
            .ok_or_else(|| anyhow::anyhow!("destination has no directory: {}", dest.display()))?;
        if protected(&dest) {
            return Err(Refused(IN_THE_INSTALL).into());
        }
        std::fs::create_dir_all(dir)?;

        let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
        tmp.write_all(&data)?;
        tmp.as_file().sync_all()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o644))?;
        }
        if protected(&dest) {
            return Err(Refused(IN_THE_INSTALL).into());
        }
        // Replaces only what the user was actually asked about. A file that appeared after they
        // answered, or one whose sanitised rename meant the dialog asked about a different name, was
        // never confirmed: refuse rather than destroy it.
        match on_existing {
            OnExisting::Replace => {
                // The consent covers the file this run started against, and the fetch since then can
                // have taken an hour. A different file here now is one nobody was asked about; it is
                // not ours to destroy: `persist` renames over whatever it finds, without looking.
                if FileIdentity::of(&dest) != before {
                    return Err(
                        Refused("the destination changed while the download was running").into(),
                    );
                }
                tmp.persist(&dest).map_err(|e| e.error)?;
                Ok(WriteOutcome::Written)
            }
            OnExisting::Skip => match tmp.persist_noclobber(&dest) {
                Ok(_) => Ok(WriteOutcome::Written),
                // The pre-fetch check already skips a file that was there when the track started;
                // this is the same outcome for one that arrived while it downloaded.
                Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => {
                    Ok(WriteOutcome::Skipped)
                }
                Err(e) => Err(e.error.into()),
            },
            OnExisting::Fail => {
                tmp.persist_noclobber(&dest).map_err(|e| e.error)?;
                Ok(WriteOutcome::Written)
            }
        }
    })
    .await?
}

#[cfg(test)]
#[path = "../../../../tests/unit/ipc/plugin/download.rs"]
mod tests;
