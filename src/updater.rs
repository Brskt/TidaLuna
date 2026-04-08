use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use cef::ImplWindow;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex as TokioMutex;
use tokio_util::sync::CancellationToken;

const GITHUB_OWNER: &str = "Brskt";
const GITHUB_REPO: &str = "TidaLuna";

const TARGET: &str = {
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    {
        "windows-amd64"
    }
    #[cfg(all(target_os = "windows", target_arch = "aarch64"))]
    {
        "windows-arm64"
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        "linux-amd64"
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    {
        "linux-arm64"
    }
    #[cfg(not(any(
        all(target_os = "windows", target_arch = "x86_64"),
        all(target_os = "windows", target_arch = "aarch64"),
        all(target_os = "linux", target_arch = "x86_64"),
        all(target_os = "linux", target_arch = "aarch64"),
    )))]
    {
        "unsupported"
    }
};

const UPDATER_EXE: &str = if cfg!(target_os = "windows") {
    "updater.exe"
} else {
    "updater"
};

// ---------------------------------------------------------------------------
// Types (shared with the updater crate via identical definitions)
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize)]
struct Manifest {
    version: String,
    min_version: String,
    target: String,
    files: BTreeMap<String, FileEntry>,
}

#[derive(Serialize, Deserialize)]
struct FileEntry {
    sha256: String,
    size: u64,
}

#[derive(Serialize, Deserialize)]
struct Journal {
    version: String,
    state: String,
    files: Vec<JournalFile>,
    #[serde(default)]
    deleted_files: Vec<String>,
}

#[derive(Serialize, Deserialize)]
struct JournalFile {
    path: String,
    backup: String,
    #[serde(default)]
    is_new: bool,
}

#[derive(Deserialize)]
struct GhRelease {
    tag_name: String,
    assets: Vec<GhAsset>,
}

#[derive(Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    size: u64,
}

/// Information about an available update, sent to the frontend.
#[derive(Serialize, Clone)]
pub(crate) struct UpdateInfo {
    pub version: String,
    pub download_size: u64,
}

// ---------------------------------------------------------------------------
// Updater state machine
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", content = "version")]
pub(crate) enum UpdaterPhase {
    Idle,
    Downloading(String),
    Ready(String),
    #[allow(dead_code)]
    Applying(String),
}

pub(crate) struct UpdaterState {
    pub phase: UpdaterPhase,
    pub task: Option<tokio::task::JoinHandle<()>>,
    pub cancel: Option<CancellationToken>,
    pub last_info: Option<UpdateInfo>,
}

pub(crate) static UPDATER_STATE: LazyLock<TokioMutex<UpdaterState>> = LazyLock::new(|| {
    TokioMutex::new(UpdaterState {
        phase: UpdaterPhase::Idle,
        task: None,
        cancel: None,
        last_info: None,
    })
});

// ---------------------------------------------------------------------------
// Startup recovery
// ---------------------------------------------------------------------------

/// Check for and resolve any interrupted update journal.
/// Must be called early in main(), before CEF init.
pub(crate) fn recover_interrupted_update() {
    let app_dir = match exe_dir() {
        Some(d) => d,
        None => return,
    };

    let journal_path = app_dir.join(".update-journal.json");
    if !journal_path.exists() {
        return;
    }

    crate::vprintln!("[UPDATER] Found update journal, recovering...");

    let data = match fs::read_to_string(&journal_path) {
        Ok(d) => d,
        Err(e) => {
            crate::vprintln!("[UPDATER] Failed to read journal: {e}");
            let _ = fs::remove_file(&journal_path);
            return;
        }
    };

    let journal: Journal = match serde_json::from_str(&data) {
        Ok(j) => j,
        Err(e) => {
            crate::vprintln!("[UPDATER] Failed to parse journal: {e}");
            let _ = fs::remove_file(&journal_path);
            return;
        }
    };

    match journal.state.as_str() {
        "pending" => {
            // Rollback: restore .bak → original
            crate::vprintln!(
                "[UPDATER] Rolling back incomplete update v{}",
                journal.version
            );
            for jf in &journal.files {
                let original = app_dir.join(&jf.path);
                let backup = app_dir.join(&jf.backup);
                if jf.is_new {
                    // No original existed — remove the newly installed file
                    fs::remove_file(&original).ok();
                } else if backup.exists() {
                    if original.exists() {
                        fs::remove_file(&original).ok();
                    }
                    fs::rename(&backup, &original).ok();
                }
            }
        }
        "committed" => {
            // Cleanup: remove .bak files + obsolete files from old layout
            crate::vprintln!(
                "[UPDATER] Cleaning up completed update v{}",
                journal.version
            );
            for jf in &journal.files {
                let backup = app_dir.join(&jf.backup);
                fs::remove_file(&backup).ok();
            }
            for del_path in &journal.deleted_files {
                if !is_safe_relative_path(del_path, &app_dir) {
                    continue;
                }
                fs::remove_file(app_dir.join(del_path)).ok();
            }
        }
        other => {
            crate::vprintln!("[UPDATER] Unknown journal state: {other}");
        }
    }

    // Clean up staging and journal
    let staging = app_dir.join(".update-staging");
    if staging.exists() {
        fs::remove_dir_all(&staging).ok();
    }
    fs::remove_file(&journal_path).ok();
    crate::vprintln!("[UPDATER] Recovery complete");
}

// ---------------------------------------------------------------------------
// Update check (async, runs on tokio)
// ---------------------------------------------------------------------------

/// Check for updates from GitHub Releases. Returns Some(UpdateInfo) if a newer
/// version is available, None otherwise.
pub(crate) async fn check_for_update() -> Option<UpdateInfo> {
    let current_version = env!("CARGO_PKG_VERSION");

    crate::vprintln!("[UPDATER] Checking for updates (current: v{current_version})...");

    let client = &*crate::state::HTTP_CLIENT;

    // Fetch latest release
    let url = format!("https://api.github.com/repos/{GITHUB_OWNER}/{GITHUB_REPO}/releases/latest");

    let resp = match client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", format!("TidaLunar/{current_version}"))
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            crate::vprintln!("[UPDATER] Failed to fetch releases: {e}");
            return None;
        }
    };

    if !resp.status().is_success() {
        crate::vprintln!("[UPDATER] GitHub API returned {}", resp.status());
        return None;
    }

    let body = match resp.text().await {
        Ok(t) => t,
        Err(e) => {
            crate::vprintln!("[UPDATER] Failed to read release body: {e}");
            return None;
        }
    };
    let release: GhRelease = match serde_json::from_str(&body) {
        Ok(r) => r,
        Err(e) => {
            crate::vprintln!("[UPDATER] Failed to parse release: {e}");
            return None;
        }
    };

    // Extract version from tag (strip leading 'v')
    let remote_version = release
        .tag_name
        .strip_prefix('v')
        .unwrap_or(&release.tag_name);

    // Anti-downgrade: only update if remote > current
    if !is_newer(remote_version, current_version) {
        crate::vprintln!("[UPDATER] Up to date (remote: v{remote_version})");
        return None;
    }

    crate::vprintln!("[UPDATER] Update available: v{remote_version}");

    // Find manifest for our platform
    let manifest_name = format!("manifest-{TARGET}.json");
    let manifest_asset = release.assets.iter().find(|a| a.name == manifest_name)?;

    let manifest_resp = match client
        .get(&manifest_asset.browser_download_url)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            crate::vprintln!("[UPDATER] Failed to download manifest: {e}");
            return None;
        }
    };

    let manifest_body = match manifest_resp.text().await {
        Ok(t) => t,
        Err(e) => {
            crate::vprintln!("[UPDATER] Failed to read manifest body: {e}");
            return None;
        }
    };
    let manifest: Manifest = match serde_json::from_str(&manifest_body) {
        Ok(m) => m,
        Err(e) => {
            crate::vprintln!("[UPDATER] Failed to parse manifest: {e}");
            return None;
        }
    };

    // Verify target matches
    if manifest.target != TARGET {
        crate::vprintln!(
            "[UPDATER] Manifest target mismatch: expected {TARGET}, got {}",
            manifest.target
        );
        return None;
    }

    let zip_name = format!("tidalunar-{TARGET}.zip");
    let download_size = release
        .assets
        .iter()
        .find(|a| a.name == zip_name)
        .map(|a| a.size)
        .unwrap_or(0);

    Some(UpdateInfo {
        version: remote_version.to_string(),
        download_size,
    })
}

/// Trigger the update check and notify the frontend if an update is available.
/// Called after login on the tokio runtime.
pub(crate) fn trigger_update_check() {
    // Check settings
    let (auto_check, skip_version) = crate::state::db().call_settings(|conn| {
        (
            crate::settings::load_update_auto_check(conn),
            crate::settings::load_update_skip_version(conn),
        )
    });
    if !auto_check {
        crate::vprintln!("[UPDATER] Auto-check disabled");
        return;
    }

    crate::state::rt_handle().spawn(async move {
        // Check if a previous session left a valid pre-downloaded staging
        if let Some(staged_version) = detect_staged_update() {
            if let Some(ref skip) = skip_version
                && *skip == staged_version
            {
                crate::vprintln!("[UPDATER] Staged v{staged_version} is dismissed, cleaning up");
                cleanup_staging();
            } else {
                let mut state = UPDATER_STATE.lock().await;
                state.phase = UpdaterPhase::Ready(staged_version.clone());
                drop(state);
                crate::vprintln!("[UPDATER] Found pre-downloaded staging for v{staged_version}");
                crate::app_state::eval_js(&format!(
                    "if(typeof window.__LUNAR_IPC_EMIT__==='function')window.__LUNAR_IPC_EMIT__('updater.ready','{staged_version}');"
                ));
                return;
            }
        }

        if let Some(info) = check_for_update().await {
            if let Some(ref skip) = skip_version
                && *skip == info.version
            {
                crate::vprintln!("[UPDATER] Skipping dismissed version v{}", info.version);
                return;
            }

            UPDATER_STATE.lock().await.last_info = Some(info.clone());

            let json = serde_json::to_string(&info).unwrap_or_default();
            crate::app_state::eval_js(&format!(
                "if(typeof window.__LUNAR_IPC_EMIT__==='function')window.__LUNAR_IPC_EMIT__('updater.available',{json});"
            ));
            crate::vprintln!(
                "[UPDATER] Notified frontend: v{} ({} bytes)",
                info.version,
                info.download_size
            );
        }
    });
}

// ---------------------------------------------------------------------------
// IPC handlers
// ---------------------------------------------------------------------------

/// Handle `updater.check` — manual check triggered by UI.
/// Returns JSON UpdateInfo or null.
pub(crate) fn handle_updater_check(callback: crate::app_state::IpcCallback) {
    crate::state::rt_handle().spawn(async move {
        let result = match check_for_update().await {
            Some(info) => serde_json::to_string(&info).unwrap_or_else(|_| "null".into()),
            None => "null".into(),
        };
        crate::ipc::plugin::ipc_callback_ok(&callback, &result);
    });
}

/// Returns immediately with "started", "download_in_progress", "already_ready", or error.
pub(crate) fn handle_updater_download(
    msg: &crate::app_state::IpcMessage,
    callback: crate::app_state::IpcCallback,
) {
    let version = msg
        .args
        .first()
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    if version.is_empty() {
        crate::ipc::plugin::ipc_callback_err(&callback, "missing version argument");
        return;
    }

    crate::state::rt_handle().spawn(async move {
        let mut state = UPDATER_STATE.lock().await;
        match &state.phase {
            UpdaterPhase::Downloading(_) => {
                crate::ipc::plugin::ipc_callback_err(&callback, "download_in_progress");
                return;
            }
            UpdaterPhase::Ready(v) if *v == version => {
                crate::ipc::plugin::ipc_callback_ok(&callback, "\"already_ready\"");
                return;
            }
            UpdaterPhase::Applying(_) => {
                crate::ipc::plugin::ipc_callback_err(&callback, "applying");
                return;
            }
            _ => {}
        }

        if let UpdaterPhase::Ready(_) = &state.phase {
            cleanup_staging();
        }

        let token = CancellationToken::new();
        state.phase = UpdaterPhase::Downloading(version.clone());
        state.cancel = Some(token.clone());

        let handle = tokio::spawn(download_update(version, token));
        state.task = Some(handle);
        drop(state);

        crate::ipc::plugin::ipc_callback_ok(&callback, "\"started\"");
    });
}

pub(crate) fn handle_updater_cancel() {
    crate::state::rt_handle().spawn(async {
        let mut state = UPDATER_STATE.lock().await;
        if let UpdaterPhase::Downloading(_) = &state.phase {
            if let Some(token) = state.cancel.take() {
                token.cancel();
            }
            if let Some(handle) = state.task.take() {
                handle.abort();
            }
            state.phase = UpdaterPhase::Idle;
            cleanup_staging();
            crate::app_state::eval_js(
                "if(typeof window.__LUNAR_IPC_EMIT__==='function')window.__LUNAR_IPC_EMIT__('updater.cancelled');",
            );
            crate::vprintln!("[UPDATER] Download cancelled by user");
        }
    });
}

pub(crate) fn handle_updater_status(callback: crate::app_state::IpcCallback) {
    crate::state::rt_handle().spawn(async move {
        let state = UPDATER_STATE.lock().await;
        #[derive(Serialize)]
        struct StatusResponse<'a> {
            #[serde(flatten)]
            phase: &'a UpdaterPhase,
            last_info: &'a Option<UpdateInfo>,
        }
        let resp = StatusResponse {
            phase: &state.phase,
            last_info: &state.last_info,
        };
        let json = serde_json::to_string(&resp).unwrap_or_else(|_| "null".into());
        crate::ipc::plugin::ipc_callback_ok(&callback, &json);
    });
}

/// Handle `updater.apply` — user confirmed, spawn updater and quit.
pub(crate) fn handle_updater_apply(msg: &crate::app_state::IpcMessage) {
    let version = msg
        .args
        .first()
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    if version.is_empty() {
        crate::vprintln!("[UPDATER] apply called without version");
        return;
    }

    let app_dir = match exe_dir() {
        Some(d) => d,
        None => {
            crate::vprintln!("[UPDATER] Cannot resolve exe dir");
            return;
        }
    };

    let updater_path = app_dir.join(UPDATER_EXE);
    if !updater_path.exists() {
        crate::vprintln!(
            "[UPDATER] Updater binary not found at {}",
            updater_path.display()
        );
        return;
    }

    let pid = std::process::id();

    crate::vprintln!(
        "[UPDATER] Spawning updater for v{version} (pid={pid}, app_dir={})",
        app_dir.display()
    );

    let manifest_name = format!("manifest-{TARGET}.json");
    let staging_manifest_path = app_dir.join(".update-staging").join(&manifest_name);
    let skip_download = match fs::read_to_string(&staging_manifest_path) {
        Ok(data) => match serde_json::from_str::<Manifest>(&data) {
            Ok(m) => m.version == version && m.target == TARGET,
            Err(_) => false,
        },
        Err(_) => false,
    };

    if !skip_download {
        let staging = app_dir.join(".update-staging");
        if staging.exists() {
            fs::remove_dir_all(&staging).ok();
        }
    }

    let mut cmd = std::process::Command::new(&updater_path);
    cmd.args([
        "--pid",
        &pid.to_string(),
        "--version",
        &version,
        "--app-dir",
        &app_dir.display().to_string(),
    ]);
    if skip_download {
        cmd.arg("--skip-download");
        crate::vprintln!("[UPDATER] Using pre-downloaded staging");
    }

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    match cmd.spawn() {
        Ok(_) => {
            crate::vprintln!("[UPDATER] Updater spawned, exiting app...");
            crate::app_state::with_state(|state| {
                state.force_quit = true;
            });
            let browser = crate::app_state::with_state(|state| state.browser.clone()).flatten();
            if let Some(window) = crate::ipc::window::get_cef_window(browser) {
                window.close();
            } else {
                cef::quit_message_loop();
            }
        }
        Err(e) => {
            crate::vprintln!("[UPDATER] Failed to spawn updater: {e}");
        }
    }
}

/// Handle `updater.dismiss` — user clicked "Skip this version".
pub(crate) fn handle_updater_dismiss(msg: &crate::app_state::IpcMessage) {
    let version = msg
        .args
        .first()
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    if !version.is_empty() {
        crate::vprintln!("[UPDATER] Dismissed version v{version}");
        crate::state::db().call_settings(move |conn| {
            crate::settings::save_update_skip_version(conn, &version);
        });
    }
}

// ---------------------------------------------------------------------------
// Pre-download (async, runs on tokio while app is open)
// ---------------------------------------------------------------------------

async fn download_update(version: String, cancel: CancellationToken) {
    let result = download_update_inner(&version, &cancel).await;

    let mut state = UPDATER_STATE.lock().await;
    match result {
        Ok(()) => {
            state.phase = UpdaterPhase::Ready(version.clone());
            state.task = None;
            state.cancel = None;
            crate::vprintln!("[UPDATER] Pre-download complete for v{version}");
            crate::app_state::eval_js(&format!(
                "if(typeof window.__LUNAR_IPC_EMIT__==='function')window.__LUNAR_IPC_EMIT__('updater.ready','{version}');"
            ));
        }
        Err(_) if cancel.is_cancelled() => {
            crate::vprintln!("[UPDATER] Download cancelled for v{version}");
            state.phase = UpdaterPhase::Idle;
            state.task = None;
            state.cancel = None;
        }
        Err(e) => {
            crate::vprintln!("[UPDATER] Download failed for v{version}: {e}");
            cleanup_staging();
            state.phase = UpdaterPhase::Idle;
            state.task = None;
            state.cancel = None;
            let msg = e.to_string().replace('\'', "\\'");
            crate::app_state::eval_js(&format!(
                "if(typeof window.__LUNAR_IPC_EMIT__==='function')window.__LUNAR_IPC_EMIT__('updater.error','{msg}');"
            ));
        }
    }
}

async fn download_update_inner(
    version: &str,
    cancel: &CancellationToken,
) -> Result<(), anyhow::Error> {
    use anyhow::{Context, bail};
    use futures_util::StreamExt;

    let app_dir = exe_dir().context("cannot resolve exe dir")?;
    let client = &*crate::state::HTTP_CLIENT;
    let current_version = env!("CARGO_PKG_VERSION");

    crate::vprintln!("[UPDATER] Fetching release {version}...");
    let url = format!(
        "https://api.github.com/repos/{GITHUB_OWNER}/{GITHUB_REPO}/releases/tags/{version}"
    );
    let resp = client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", format!("TidaLunar/{current_version}"))
        .send()
        .await
        .context("fetch release")?;
    if !resp.status().is_success() {
        bail!("GitHub API returned {}", resp.status());
    }
    let body = resp.text().await.context("read release body")?;
    let release: GhRelease = serde_json::from_str(&body).context("parse release")?;

    if cancel.is_cancelled() {
        bail!("cancelled");
    }

    let manifest_name = format!("manifest-{TARGET}.json");
    let sig_name = format!("{manifest_name}.sig");

    let manifest_asset = release
        .assets
        .iter()
        .find(|a| a.name == manifest_name)
        .context(format!("release missing {manifest_name}"))?;
    let sig_asset = release
        .assets
        .iter()
        .find(|a| a.name == sig_name)
        .context(format!("release missing {sig_name}"))?;

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

    if manifest.target != TARGET {
        bail!(
            "manifest target mismatch: expected {TARGET}, got {}",
            manifest.target
        );
    }

    if cancel.is_cancelled() {
        bail!("cancelled");
    }

    let staging = app_dir.join(".update-staging");
    if staging.exists() {
        fs::remove_dir_all(&staging).ok();
    }
    fs::create_dir_all(&staging).context("create staging dir")?;

    let zip_name = format!("tidalunar-{TARGET}.zip");
    let zip_asset = release
        .assets
        .iter()
        .find(|a| a.name == zip_name)
        .context(format!("release missing {zip_name}"))?;

    crate::vprintln!("[UPDATER] Downloading {zip_name}...");
    let zip_resp = client
        .get(&zip_asset.browser_download_url)
        .send()
        .await
        .context("download zip")?;

    if !zip_resp.status().is_success() {
        bail!("zip download returned {}", zip_resp.status());
    }

    let zip_path = staging.join("update.zip");
    {
        let mut file = fs::File::create(&zip_path).context("create zip file")?;
        let mut stream = zip_resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            if cancel.is_cancelled() {
                bail!("cancelled");
            }
            let chunk = chunk.context("read zip chunk")?;
            std::io::Write::write_all(&mut file, &chunk).context("write zip chunk")?;
        }
    }

    if cancel.is_cancelled() {
        bail!("cancelled");
    }

    crate::vprintln!("[UPDATER] Extracting...");
    {
        let zip = zip_path.clone();
        let dest = staging.clone();
        tokio::task::spawn_blocking(move || extract_zip(&zip, &dest))
            .await
            .context("extract task panicked")??;
    }
    fs::remove_file(&zip_path).ok();

    if cancel.is_cancelled() {
        bail!("cancelled");
    }

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

    // Written last — acts as the completion marker for --skip-download
    fs::write(staging.join(&manifest_name), &manifest_bytes).context("write staged manifest")?;
    fs::write(staging.join(&sig_name), &sig_bytes).context("write staged signature")?;

    crate::vprintln!("[UPDATER] Staging complete for v{version}");
    Ok(())
}

/// Extract a zip archive into a destination directory.
fn extract_zip(zip_path: &Path, dest: &Path) -> Result<(), anyhow::Error> {
    use anyhow::Context;

    let file = fs::File::open(zip_path).context("open zip")?;
    let mut archive = zip::ZipArchive::new(file).context("parse zip")?;

    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).context("zip entry")?;
        let name = entry.name().to_string();

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

/// Remove the staging directory (best effort).
fn cleanup_staging() {
    if let Some(app_dir) = exe_dir() {
        let staging = app_dir.join(".update-staging");
        if staging.exists() {
            fs::remove_dir_all(&staging).ok();
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Returns the staged version if a valid pre-downloaded update exists.
fn detect_staged_update() -> Option<String> {
    let app_dir = exe_dir()?;
    let manifest_name = format!("manifest-{TARGET}.json");
    let manifest_path = app_dir.join(".update-staging").join(&manifest_name);
    let data = fs::read_to_string(&manifest_path).ok()?;
    let manifest: Manifest = serde_json::from_str(&data).ok()?;
    if manifest.target != TARGET {
        return None;
    }
    let current = env!("CARGO_PKG_VERSION");
    if is_newer(&manifest.version, current) {
        Some(manifest.version)
    } else {
        None
    }
}

/// Reject absolute paths and directory-escape components.
fn is_safe_relative_path(rel: &str, base: &Path) -> bool {
    let p = Path::new(rel);
    if p.is_absolute() {
        return false;
    }
    for c in p.components() {
        if matches!(
            c,
            std::path::Component::ParentDir | std::path::Component::Prefix(_)
        ) {
            return false;
        }
    }
    base.join(rel).starts_with(base)
}

fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
}

fn sha256_file(path: &Path) -> Result<String, std::io::Error> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(base16ct::lower::encode_string(&hasher.finalize()))
}

/// Simple semver comparison: returns true if `remote` > `current`.
/// Strips pre-release suffixes (e.g. "-alpha") before comparing numeric parts.
fn is_newer(remote: &str, current: &str) -> bool {
    let parse = |s: &str| -> (u32, u32, u32) {
        // Strip pre-release suffix: "0.0.2-alpha" → "0.0.2"
        let numeric = s.split('-').next().unwrap_or(s);
        let mut parts = numeric.split('.');
        let major = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
        let minor = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
        let patch = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
        (major, minor, patch)
    };
    parse(remote) > parse(current)
}
