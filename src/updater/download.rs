use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use futures_util::StreamExt;
use tokio_util::bytes::Bytes;
use tokio_util::sync::CancellationToken;

use super::types::{GhRelease, Manifest, UPDATER_STATE, UpdaterPhase};
use super::util::{exe_dir, fetch_gh_release, sha256_file};
// Extraction is zip on Windows and tar.gz on Linux; anywhere else bails before
// it needs to vet an entry's path.
#[cfg(any(target_os = "windows", target_os = "linux"))]
use super::util::is_safe_relative_path;

macro_rules! check_cancel {
    ($cancel:expr) => {
        if $cancel.is_cancelled() {
            bail!("cancelled");
        }
    };
}

pub(super) async fn download_update(version: String, cancel: CancellationToken) {
    let result = download_update_inner(&version, &cancel).await;

    let mut state = UPDATER_STATE.lock().await;
    match result {
        Ok(()) => {
            state.phase = UpdaterPhase::Ready(version.clone());
            state.reset_task();
            crate::vprintln!("[UPDATER] Pre-download complete for v{version}");
            crate::app_state::emit_ipc_event_with_args("updater.ready", &[&version]);
        }
        Err(_) if cancel.is_cancelled() => {
            crate::vprintln!("[UPDATER] Download cancelled for v{version}");
            state.reset_to_idle();
        }
        Err(e) => {
            crate::vprintln!("[UPDATER] Download failed for v{version}: {e}");
            cleanup_staging();
            state.reset_to_idle();
            let msg = e.to_string().replace('\'', "\\'");
            crate::app_state::emit_ipc_event_with_args("updater.error", &[&msg]);
        }
    }
}

async fn download_update_inner(
    version: &str,
    cancel: &CancellationToken,
) -> Result<(), anyhow::Error> {
    let app_dir = exe_dir().context("cannot resolve exe dir")?;
    let client = &*crate::state::HTTP_CLIENT;

    crate::vprintln!("[UPDATER] Fetching release {version}...");
    let release = fetch_gh_release(client, &format!("releases/tags/{version}")).await?;
    check_cancel!(cancel);

    let (manifest_bytes, sig_bytes, manifest): (Bytes, Bytes, Manifest) =
        download_manifest_and_sig(client, &release).await?;
    super::verify::verify_manifest_signature(&manifest_bytes, &sig_bytes)
        .context("manifest signature invalid")?;
    manifest.verify_target()?;
    #[cfg(target_os = "linux")]
    super::util::enforce_sandbox_protocol_gate(&manifest)?;

    // Skip-migration floor; the signature is verified above and min_version is trusted.
    let current = env!("CARGO_PKG_VERSION");
    if !super::util::meets_min_version(current, &manifest.min_version) {
        anyhow::bail!(
            "update v{} requires installed version >= v{}, but have v{}",
            manifest.version,
            manifest.min_version,
            current
        );
    }
    // Anti-rollback: reject any target not newer than the high-water mark.
    let mark = super::highwater::load(&crate::state::cache_data_dir());
    if !super::util::is_newer(&manifest.version, &mark) {
        anyhow::bail!(
            "update v{} is not newer than the highest installed version v{} (anti-rollback)",
            manifest.version,
            mark
        );
    }
    check_cancel!(cancel);

    // A delta omits every file whose hash CI found unchanged between the two releases, a
    // claim about the releases and not about this disk. Verification below can therefore
    // reject the local base, in which case the same update retries against the full archive;
    // failing hard would let one damaged file block every future update from inside the app.
    let mut use_delta = manifest.delta_from.as_deref() == Some(current);
    let staging = loop {
        let staging = prepare_staging_dir(&app_dir)?;

        let (archive_name, archive_asset) = {
            let delta_name = super::delta_archive_name(version);
            let delta = if use_delta {
                release.assets.iter().find(|a| a.name == delta_name)
            } else {
                None
            };
            match delta {
                Some(a) => {
                    crate::vprintln!("[UPDATER] Using delta from v{current}");
                    (delta_name, a)
                }
                None => {
                    use_delta = false;
                    let full = super::archive_name(version);
                    let a = release
                        .assets
                        .iter()
                        .find(|x| x.name == full)
                        .with_context(|| format!("release missing {full}"))?;
                    (full, a)
                }
            }
        };

        let archive_path = staging.join(&archive_name);
        stream_to_file(
            client,
            &archive_asset.browser_download_url,
            &archive_path,
            cancel,
        )
        .await?;
        check_cancel!(cancel);

        crate::vprintln!("[UPDATER] Extracting...");
        {
            let archive = archive_path.clone();
            let dest = staging.clone();
            tokio::task::spawn_blocking(move || extract_archive(&archive, &dest))
                .await
                .context("extract task panicked")??;
        }
        fs::remove_file(&archive_path).ok();
        check_cancel!(cancel);

        // Off the runtime, like `extract_archive` above: verifying a delta hashes every file
        // the archive did not ship, which is most of what CEF installs, and would stall every
        // other task on the runtime, playback segment fetches included. `spawn_blocking`
        // rather than `block_in_place` because work behind the latter cannot be cancelled and
        // this path is built around a `CancellationToken`. Owned arguments because
        // `spawn_blocking` demands `'static`, which is also why the list below is rebuilt per
        // iteration rather than hoisted; ownership can only be handed over once, and the loop
        // runs twice at most, on a delta-base mismatch.
        let expected: Vec<(String, String)> = manifest
            .files
            .iter()
            .map(|(path, entry)| (path.clone(), entry.sha256.clone()))
            .collect();
        let verify = {
            let staging = staging.clone();
            let app_dir = app_dir.clone();
            tokio::task::spawn_blocking(move || {
                verify_staged_files(&expected, &staging, &app_dir, use_delta)
            })
            .await
            .context("verify task panicked")?
        };
        match verify {
            Ok(()) => break staging,
            // Only reachable while `use_delta` is true, and the retry clears it; this
            // falls back exactly once.
            Err(StagingError::DeltaBaseMismatch(what)) => {
                crate::verr!(
                    "[UPDATER] Local base does not match the delta's assumption ({what}); \
                     re-downloading the full archive"
                );
                use_delta = false;
            }
            Err(StagingError::Fatal(e)) => return Err(e),
        }
    };

    let manifest_name = super::manifest_name();
    let sig_name = format!("{manifest_name}.sig");
    // Written last - acts as the completion marker for --skip-download
    fs::write(staging.join(&manifest_name), &manifest_bytes).context("write staged manifest")?;
    fs::write(staging.join(&sig_name), &sig_bytes).context("write staged signature")?;

    crate::vprintln!("[UPDATER] Staging complete for v{version}");
    Ok(())
}

/// Cap on the metadata downloads (manifest JSON + detached signature). Small
/// first-party files; the bound stops an oversized body from being buffered in
/// the live app process before the signature can be verified.
const MAX_MANIFEST_BYTES: u64 = 16 * 1024 * 1024;

/// Add `add` to the running byte total, failing if it would exceed `max`.
fn bump_capped(running: u64, add: usize, max: u64) -> Result<u64, anyhow::Error> {
    let next = running.saturating_add(add as u64);
    if next > max {
        bail!("download exceeds the {max}-byte cap");
    }
    Ok(next)
}

/// Fetch `url` into memory, failing if the body exceeds `max` bytes. The cap is
/// the only pre-verification defense, since the signature can only be checked
/// once the bytes are received.
async fn fetch_capped(
    client: &reqwest::Client,
    url: &str,
    max: u64,
    what: &str,
) -> Result<Bytes, anyhow::Error> {
    let resp = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("download {what}"))?;
    let mut buf = Vec::new();
    let mut total = 0u64;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.with_context(|| format!("read {what} bytes"))?;
        total = bump_capped(total, chunk.len(), max)?;
        buf.extend_from_slice(&chunk);
    }
    Ok(Bytes::from(buf))
}

async fn download_manifest_and_sig(
    client: &reqwest::Client,
    release: &GhRelease,
) -> Result<(Bytes, Bytes, Manifest), anyhow::Error> {
    let manifest_name = super::manifest_name();
    let sig_name = format!("{manifest_name}.sig");

    let manifest_asset = release
        .assets
        .iter()
        .find(|a| a.name == manifest_name)
        .with_context(|| format!("release missing {manifest_name}"))?;
    let sig_asset = release
        .assets
        .iter()
        .find(|a| a.name == sig_name)
        .with_context(|| format!("release missing {sig_name}"))?;

    crate::vprintln!("[UPDATER] Downloading manifest + signature...");
    let (manifest_bytes, sig_bytes) = tokio::try_join!(
        fetch_capped(
            client,
            &manifest_asset.browser_download_url,
            MAX_MANIFEST_BYTES,
            "manifest",
        ),
        fetch_capped(
            client,
            &sig_asset.browser_download_url,
            MAX_MANIFEST_BYTES,
            "signature",
        ),
    )?;

    let manifest: Manifest =
        serde_json::from_slice(&manifest_bytes).context("invalid manifest JSON")?;

    Ok((manifest_bytes, sig_bytes, manifest))
}

/// Hard ceiling on the archive download. Far above any real release; bounds disk
/// and memory if an oversized body is streamed before the staged-file hash check
/// can reject it.
const MAX_ARCHIVE_BYTES: u64 = 2 * 1024 * 1024 * 1024;

async fn stream_to_file(
    client: &reqwest::Client,
    url: &str,
    dest: &Path,
    cancel: &CancellationToken,
) -> Result<(), anyhow::Error> {
    let name = dest
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("update");
    crate::vprintln!("[UPDATER] Downloading {name}...");

    let resp = client.get(url).send().await.context("download archive")?;

    if !resp.status().is_success() {
        bail!("archive download returned {}", resp.status());
    }

    let mut file = fs::File::create(dest).context("create archive file")?;
    let mut stream = resp.bytes_stream();
    let mut total = 0u64;
    while let Some(chunk) = stream.next().await {
        check_cancel!(cancel);
        let chunk = chunk.context("read archive chunk")?;
        total = bump_capped(total, chunk.len(), MAX_ARCHIVE_BYTES)?;
        std::io::Write::write_all(&mut file, &chunk).context("write archive chunk")?;
    }

    Ok(())
}

/// Why staging verification failed, split by what the caller can do about it.
enum StagingError {
    /// A file the delta omitted as "unchanged" is absent or does not match the manifest
    /// hash on disk. The downloaded bytes are fine; the assumed base is not: the full
    /// archive will succeed where this delta cannot.
    DeltaBaseMismatch(String),
    /// The bytes we just downloaded are wrong. Retrying the same thing cannot help.
    Fatal(anyhow::Error),
}

/// `expected` pairs each manifest path with the SHA-256 the signed manifest records for it.
/// Blocking and I/O-bound; call it through `spawn_blocking`.
fn verify_staged_files(
    expected: &[(String, String)],
    staging: &Path,
    app_dir: &Path,
    is_delta: bool,
) -> Result<(), StagingError> {
    crate::vprintln!("[UPDATER] Verifying staged files...");
    for (rel_path, expected_hash) in expected {
        let staged_path = staging.join(rel_path);
        if !staged_path.exists() {
            if !is_delta {
                return Err(StagingError::Fatal(anyhow::anyhow!(
                    "staged file missing from full archive: {rel_path}"
                )));
            }
            // The signature makes the expected hash trustworthy, but what it attests is the
            // hash table, never the state of this disk. Nothing else in the update reads the
            // local copy of a file the delta skipped. Bit rot, an incomplete rollback or
            // local tampering rode through and the update still reported success.
            let local_path = app_dir.join(rel_path);
            let local_hash = match sha256_file(&local_path) {
                Ok(h) => h,
                Err(e) => {
                    return Err(StagingError::DeltaBaseMismatch(format!("{rel_path}: {e}")));
                }
            };
            if local_hash != *expected_hash {
                return Err(StagingError::DeltaBaseMismatch(format!(
                    "{rel_path}: expected {expected_hash}, found {local_hash}"
                )));
            }
            continue;
        }
        let hash = sha256_file(&staged_path)
            .with_context(|| format!("hash staged file {rel_path}"))
            .map_err(StagingError::Fatal)?;
        if hash != *expected_hash {
            return Err(StagingError::Fatal(anyhow::anyhow!(
                "staged file {rel_path} hash mismatch: expected {expected_hash}, got {hash}"
            )));
        }
    }
    Ok(())
}

fn prepare_staging_dir(app_dir: &Path) -> Result<PathBuf, anyhow::Error> {
    let staging = app_dir.join(".update-staging");
    if staging.exists() {
        fs::remove_dir_all(&staging).ok();
    }
    fs::create_dir_all(&staging).context("create staging dir")?;
    Ok(staging)
}

fn extract_archive(archive_path: &Path, dest: &Path) -> Result<(), anyhow::Error> {
    #[cfg(target_os = "windows")]
    return extract_zip(archive_path, dest);
    #[cfg(target_os = "linux")]
    return extract_tar_gz(archive_path, dest);
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        let _ = (archive_path, dest);
        bail!("update extraction unsupported on this platform");
    }
}

#[cfg(target_os = "windows")]
fn extract_zip(zip_path: &Path, dest: &Path) -> Result<(), anyhow::Error> {
    let file = fs::File::open(zip_path).context("open zip")?;
    let mut archive = zip::ZipArchive::new(file).context("parse zip")?;

    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).context("zip entry")?;
        let name = entry.name().to_string();

        if !is_safe_relative_path(&name, dest) {
            bail!("zip entry has unsafe path: {name}");
        }

        if entry.is_dir() {
            fs::create_dir_all(dest.join(&name)).ok();
            continue;
        }

        let out_path = dest.join(&name);
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent).ok();
        }

        let mut out_file = fs::File::create(&out_path)
            .with_context(|| format!("create {}", out_path.display()))?;
        std::io::copy(&mut entry, &mut out_file).with_context(|| format!("extract {name}"))?;
    }
    Ok(())
}

/// Extract a `.tar.gz` whose entries are wrapped in a single top-level
/// directory (the portable tarball layout), stripping it and leaving files
/// land at `dest` root to match the manifest's relative paths. Unix modes from
/// the tar header are preserved (keeps chrome-sandbox executable).
#[cfg(target_os = "linux")]
fn extract_tar_gz(archive_path: &Path, dest: &Path) -> Result<(), anyhow::Error> {
    let file = fs::File::open(archive_path).context("open tar.gz")?;
    let decoder = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(decoder);

    for entry in archive.entries().context("read tar entries")? {
        let mut entry = entry.context("tar entry")?;
        let path = entry.path().context("tar entry path")?.into_owned();

        // Drop the single leading component (tidalunar_{version}_linux_{arch}/).
        let mut comps = path.components();
        comps.next();
        let rel = comps.as_path();
        if rel.as_os_str().is_empty() {
            continue;
        }

        let rel_str = rel.to_string_lossy();
        if !is_safe_relative_path(&rel_str, dest) {
            bail!("tar entry has unsafe path: {}", path.display());
        }

        let out_path = dest.join(rel);
        let etype = entry.header().entry_type();
        if etype.is_dir() {
            fs::create_dir_all(&out_path).ok();
            continue;
        }
        if !etype.is_file() {
            // The payload is plain files only. Reject symlinks/hardlinks/devices:
            // in an archive that isn't hash-bound before extraction, they are a
            // traversal vector (a symlink target escapes is_safe_relative_path).
            bail!("tar entry {rel_str} has disallowed type {etype:?}");
        }
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent).ok();
        }
        entry
            .unpack(&out_path)
            .with_context(|| format!("extract {rel_str}"))?;
    }
    Ok(())
}

pub(super) fn cleanup_staging() {
    if let Some(app_dir) = exe_dir() {
        let staging = app_dir.join(".update-staging");
        if staging.exists() {
            fs::remove_dir_all(&staging).ok();
        }
    }
}

#[cfg(test)]
#[path = "../../tests/unit/updater/download/cap_tests.rs"]
mod cap_tests;
