use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use futures_util::StreamExt;
use tokio_util::bytes::Bytes;
use tokio_util::sync::CancellationToken;

use super::types::{GhRelease, Manifest, UPDATER_STATE, UpdaterPhase};
use super::util::{exe_dir, fetch_gh_release, is_safe_relative_path, sha256_file};

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
    check_cancel!(cancel);

    let staging = prepare_staging_dir(&app_dir)?;

    let current = env!("CARGO_PKG_VERSION");
    let use_delta = manifest.delta_from.as_deref() == Some(current);
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

    verify_staged_files(&manifest, &staging, use_delta)?;

    let manifest_name = super::manifest_name();
    let sig_name = format!("{manifest_name}.sig");
    // Written last - acts as the completion marker for --skip-download
    fs::write(staging.join(&manifest_name), &manifest_bytes).context("write staged manifest")?;
    fs::write(staging.join(&sig_name), &sig_bytes).context("write staged signature")?;

    crate::vprintln!("[UPDATER] Staging complete for v{version}");
    Ok(())
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
        async {
            client
                .get(&manifest_asset.browser_download_url)
                .send()
                .await
                .context("download manifest")?
                .bytes()
                .await
                .context("read manifest bytes")
        },
        async {
            client
                .get(&sig_asset.browser_download_url)
                .send()
                .await
                .context("download signature")?
                .bytes()
                .await
                .context("read signature bytes")
        },
    )?;

    let manifest: Manifest =
        serde_json::from_slice(&manifest_bytes).context("invalid manifest JSON")?;

    Ok((manifest_bytes, sig_bytes, manifest))
}

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
    while let Some(chunk) = stream.next().await {
        check_cancel!(cancel);
        let chunk = chunk.context("read archive chunk")?;
        std::io::Write::write_all(&mut file, &chunk).context("write archive chunk")?;
    }

    Ok(())
}

fn verify_staged_files(
    manifest: &Manifest,
    staging: &Path,
    is_delta: bool,
) -> Result<(), anyhow::Error> {
    crate::vprintln!("[UPDATER] Verifying staged files...");
    for (rel_path, entry) in &manifest.files {
        let staged_path = staging.join(rel_path);
        if !staged_path.exists() {
            if is_delta {
                // Delta archive: unchanged files are not shipped; the existing
                // local copy is trusted (current version == manifest.delta_from).
                continue;
            }
            bail!("staged file missing from full archive: {rel_path}");
        }
        let hash =
            sha256_file(&staged_path).with_context(|| format!("hash staged file {rel_path}"))?;
        if hash != entry.sha256 {
            bail!(
                "staged file {rel_path} hash mismatch: expected {}, got {hash}",
                entry.sha256
            );
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
/// directory (the portable tarball layout), stripping that directory so files
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
