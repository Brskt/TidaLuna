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
    manifest.verify_target()?;
    check_cancel!(cancel);

    let staging = prepare_staging_dir(&app_dir)?;

    let zip_name = super::zip_name();
    let zip_asset = release
        .assets
        .iter()
        .find(|a| a.name == zip_name)
        .with_context(|| format!("release missing {zip_name}"))?;

    let zip_path = staging.join("update.zip");
    stream_zip_to_staging(client, &zip_asset.browser_download_url, &zip_path, cancel).await?;
    check_cancel!(cancel);

    crate::vprintln!("[UPDATER] Extracting...");
    {
        let zip = zip_path.clone();
        let dest = staging.clone();
        tokio::task::spawn_blocking(move || extract_zip(&zip, &dest))
            .await
            .context("extract task panicked")??;
    }
    fs::remove_file(&zip_path).ok();
    check_cancel!(cancel);

    verify_staged_files(&manifest, &staging)?;

    let manifest_name = super::manifest_name();
    let sig_name = format!("{manifest_name}.sig");
    // Written last — acts as the completion marker for --skip-download
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

async fn stream_zip_to_staging(
    client: &reqwest::Client,
    zip_url: &str,
    zip_path: &Path,
    cancel: &CancellationToken,
) -> Result<(), anyhow::Error> {
    let zip_name = zip_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("update.zip");
    crate::vprintln!("[UPDATER] Downloading {zip_name}...");

    let zip_resp = client.get(zip_url).send().await.context("download zip")?;

    if !zip_resp.status().is_success() {
        bail!("zip download returned {}", zip_resp.status());
    }

    let mut file = fs::File::create(zip_path).context("create zip file")?;
    let mut stream = zip_resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        check_cancel!(cancel);
        let chunk = chunk.context("read zip chunk")?;
        std::io::Write::write_all(&mut file, &chunk).context("write zip chunk")?;
    }

    Ok(())
}

fn verify_staged_files(manifest: &Manifest, staging: &Path) -> Result<(), anyhow::Error> {
    crate::vprintln!("[UPDATER] Verifying staged files...");
    for (rel_path, entry) in &manifest.files {
        let staged_path = staging.join(rel_path);
        if !staged_path.exists() {
            bail!("staged file missing: {rel_path}");
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

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Some(mode) = entry.unix_mode() {
                fs::set_permissions(&out_path, fs::Permissions::from_mode(mode)).ok();
            }
        }
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
