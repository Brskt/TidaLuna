use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use cef::ImplWindow;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

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
}

/// Information about an available update, sent to the frontend.
#[derive(Serialize, Clone)]
pub(crate) struct UpdateInfo {
    pub version: String,
    pub changed_files: usize,
    pub download_size: u64,
}

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
            // Cleanup: remove .bak files
            crate::vprintln!(
                "[UPDATER] Cleaning up completed update v{}",
                journal.version
            );
            for jf in &journal.files {
                let backup = app_dir.join(&jf.backup);
                fs::remove_file(&backup).ok();
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
    let app_dir = exe_dir()?;
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

    // Compare local files against manifest to determine what changed
    let mut changed_files = 0usize;
    let mut download_size = 0u64;

    for (rel_path, entry) in &manifest.files {
        let local_path = app_dir.join(rel_path);
        let needs_update = if local_path.exists() {
            match sha256_file(&local_path) {
                Ok(hash) => hash != entry.sha256,
                Err(_) => true,
            }
        } else {
            true
        };

        if needs_update {
            changed_files += 1;
            download_size += entry.size;
        }
    }

    if changed_files == 0 {
        crate::vprintln!("[UPDATER] All files match despite version bump");
        return None;
    }

    Some(UpdateInfo {
        version: remote_version.to_string(),
        changed_files,
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
        if let Some(info) = check_for_update().await {
            // Skip if user dismissed this version
            if let Some(ref skip) = skip_version
                && *skip == info.version
            {
                crate::vprintln!("[UPDATER] Skipping dismissed version v{}", info.version);
                return;
            }

            // Notify frontend
            let json = serde_json::to_string(&info).unwrap_or_default();
            crate::app_state::eval_js(&format!(
                "if(typeof window.__LUNAR_IPC_EMIT__==='function')window.__LUNAR_IPC_EMIT__('updater.available',{json});"
            ));
            crate::vprintln!(
                "[UPDATER] Notified frontend: v{} ({} files, {} bytes)",
                info.version,
                info.changed_files,
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

    let updater_path = app_dir.join("updater").join(UPDATER_EXE);
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

    let mut cmd = std::process::Command::new(&updater_path);
    cmd.args([
        "--pid",
        &pid.to_string(),
        "--version",
        &version,
        "--app-dir",
        &app_dir.display().to_string(),
    ]);

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    match cmd.spawn() {
        Ok(_) => {
            crate::vprintln!("[UPDATER] Updater spawned, exiting app...");
            // Force quit the app
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
// Helpers
// ---------------------------------------------------------------------------

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
    Ok(format!("{:x}", hasher.finalize()))
}

/// Simple semver comparison: returns true if `remote` > `current`.
fn is_newer(remote: &str, current: &str) -> bool {
    let parse = |s: &str| -> (u32, u32, u32) {
        let mut parts = s.split('.');
        let major = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
        let minor = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
        let patch = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
        (major, minor, patch)
    };
    parse(remote) > parse(current)
}
