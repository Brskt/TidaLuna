use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

mod cleanup;
#[cfg(target_os = "windows")]
mod lock;

// ---------------------------------------------------------------------------
// Public key for update signature verification
// Replace with actual key from `cargo xtask generate-keypair`
// ---------------------------------------------------------------------------
const UPDATE_PUBLIC_KEY: [u8; 32] = [
    104, 175, 158, 150, 215, 73, 36, 25, 193, 27, 127, 255, 238, 170, 136, 130, 171, 47, 180, 243,
    2, 222, 95, 197, 57, 244, 218, 25, 117, 200, 42, 57,
];

const GITHUB_OWNER: &str = "Brskt";
const GITHUB_REPO: &str = "TidaLuna";

const EXE_NAME: &str = if cfg!(target_os = "windows") {
    "tidalunar.exe"
} else {
    "tidalunar"
};

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

/// Delta archive asset name for a version, e.g.
/// `tidalunar_0.0.6-alpha_update_win32_x64.zip`.
fn delta_archive_name(version: &str) -> String {
    format!("tidalunar_{version}_update_{ARCHIVE_SUFFIX}")
}

/// Platform suffix for the release archive (`{os}_{arch}.{ext}`). Mirrors
/// `src/updater/mod.rs::ARCHIVE_SUFFIX`. Windows ships a flat `.zip`, Linux a
/// `.tar.gz` whose entries are wrapped in one top-level directory.
const ARCHIVE_SUFFIX: &str = {
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    {
        "win32_x64.zip"
    }
    #[cfg(all(target_os = "windows", target_arch = "aarch64"))]
    {
        "win32_arm64.zip"
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        "linux_amd64.tar.gz"
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    {
        "linux_arm64.tar.gz"
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

// ---------------------------------------------------------------------------
// Sandbox protocol gate (Linux .deb cross-track compatibility)
// ---------------------------------------------------------------------------

/// Read /usr/lib/tidalunar/SANDBOX_PROTOCOL_VERSION. `None` = file absent
/// (not a packaged .deb install: tar.gz/dev, gate N/A); `Some(0)` = present but
/// malformed. Mirrors src/updater/util.rs::read_system_sandbox_protocol; the
/// standalone updater is intentionally dependency-free so this is duplicated.
#[cfg(target_os = "linux")]
fn read_system_sandbox_protocol() -> Option<u32> {
    fs::read_to_string("/usr/lib/tidalunar/SANDBOX_PROTOCOL_VERSION")
        .ok()
        .map(|s| s.trim().parse::<u32>().unwrap_or(0))
}

#[cfg(target_os = "linux")]
fn enforce_sandbox_protocol_gate(manifest: &Manifest) -> Result<(), anyhow::Error> {
    use anyhow::bail;
    // No system protocol file -> not a .deb install -> gate does not apply.
    let Some(system) = read_system_sandbox_protocol() else {
        return Ok(());
    };
    let required = manifest.sandbox_protocol_required.unwrap_or(0);
    if required > system {
        bail!(
            "Update v{} requires sandbox helper protocol {}, but system has {}. \
             Run 'sudo apt upgrade tidalunar' and re-launch.",
            manifest.version,
            required,
            system,
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize)]
pub(crate) struct Manifest {
    pub(crate) version: String,
    pub(crate) min_version: String,
    pub(crate) target: String,
    pub(crate) files: BTreeMap<String, FileEntry>,
    /// Linux-only: minimum value of `/usr/lib/tidalunar/SANDBOX_PROTOCOL_VERSION`
    /// the system bootstrap must have for this update to be safe to apply.
    /// Defaults to `None` for backwards compatibility with manifests generated
    /// before the field was added (2026-04). Mirrors the field in
    /// `src/updater/types.rs::Manifest` (the in-app updater).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) sandbox_protocol_required: Option<u32>,
    /// Mirrors `src/updater/types.rs::Manifest::delta_from`: the previous
    /// release this update's delta archive diffs against, or `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) delta_from: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct FileEntry {
    sha256: String,
    size: u64,
}

#[derive(Serialize, Deserialize)]
struct Journal {
    version: String,
    state: JournalState,
    files: Vec<JournalFile>,
    #[serde(default)]
    deleted_files: Vec<String>,
}

#[derive(Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum JournalState {
    Pending,
    Committed,
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
    #[allow(dead_code)]
    tag_name: String,
    assets: Vec<GhAsset>,
}

#[derive(Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
}

// ---------------------------------------------------------------------------
// Args
// ---------------------------------------------------------------------------

struct Args {
    pid: u32,
    version: String,
    app_dir: PathBuf,
    skip_download: bool,
}

fn parse_args() -> Result<Args> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut pid = None;
    let mut version = None;
    let mut app_dir = None;
    let mut skip_download = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--pid" => {
                i += 1;
                pid = Some(
                    args.get(i)
                        .context("--pid requires a value")?
                        .parse::<u32>()?,
                );
            }
            "--version" => {
                i += 1;
                version = Some(args.get(i).context("--version requires a value")?.clone());
            }
            "--app-dir" => {
                i += 1;
                app_dir = Some(PathBuf::from(
                    args.get(i).context("--app-dir requires a value")?,
                ));
            }
            "--skip-download" => {
                skip_download = true;
            }
            other => bail!("unknown argument: {other}"),
        }
        i += 1;
    }

    Ok(Args {
        pid: pid.context("--pid is required")?,
        version: version.context("--version is required")?,
        app_dir: app_dir.context("--app-dir is required")?,
        skip_download,
    })
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() {
    // Cleanup-stale bypasses run()'s mutex/relaunch/MessageBox path - the
    // installer that invokes this already holds the mutex.
    if std::env::args().nth(1).as_deref() == Some("--cleanup-stale") {
        std::process::exit(run_cleanup_stale());
    }

    if std::env::args().len() <= 1 {
        return;
    }

    if let Err(e) = run() {
        eprintln!("Update failed: {e:#}");
        show_error(&format!("TidaLunar update failed:\n{e:#}"));
        // Always try to relaunch the app even on failure
        let app_dir = std::env::args()
            .skip(1)
            .collect::<Vec<_>>()
            .windows(2)
            .find(|w| w[0] == "--app-dir")
            .map(|w| PathBuf::from(&w[1]));
        if let Some(dir) = app_dir {
            let _ = relaunch(&dir);
        }
        std::process::exit(1);
    }
}

fn run_cleanup_stale() -> i32 {
    let args: Vec<String> = std::env::args().skip(2).collect();
    let mut app_dir: Option<PathBuf> = None;
    let mut old_manifest: Option<PathBuf> = None;
    let mut new_manifest: Option<PathBuf> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--app-dir" => {
                i += 1;
                let Some(v) = args.get(i) else {
                    eprintln!("[cleanup-stale] --app-dir requires a value");
                    return 2;
                };
                app_dir = Some(PathBuf::from(v));
            }
            "--old-manifest" => {
                i += 1;
                let Some(v) = args.get(i) else {
                    eprintln!("[cleanup-stale] --old-manifest requires a value");
                    return 2;
                };
                old_manifest = Some(PathBuf::from(v));
            }
            "--new-manifest" => {
                i += 1;
                let Some(v) = args.get(i) else {
                    eprintln!("[cleanup-stale] --new-manifest requires a value");
                    return 2;
                };
                new_manifest = Some(PathBuf::from(v));
            }
            other => {
                eprintln!("[cleanup-stale] unknown argument: {other}");
                return 2;
            }
        }
        i += 1;
    }
    let (Some(app), Some(old), Some(new)) = (app_dir, old_manifest, new_manifest) else {
        eprintln!("[cleanup-stale] need --app-dir, --old-manifest, --new-manifest");
        return 2;
    };
    match cleanup::cleanup_stale(&app, &old, &new) {
        Ok(n) => {
            eprintln!("[cleanup-stale] removed {n} stale file(s)");
            0
        }
        Err(e) => {
            eprintln!("[cleanup-stale] failed: {e:#}");
            1
        }
    }
}

fn run() -> Result<()> {
    let args = parse_args()?;

    // Cross-process install/update mutex. SID-scoped Global\ name; serialises
    // against an installer holding the same lock. RAII via Drop: released on
    // function return / process exit. Cleanup-mode bypasses this path entirely
    // (see fn main).
    #[cfg(target_os = "windows")]
    let _install_lock = match lock::try_acquire()? {
        Some(lock) => lock,
        None => {
            show_error("Another TidaLunar installer is running. Update aborted.");
            bail!("install/update mutex held by another process");
        }
    };

    // 1. Wait for the app to exit
    eprintln!("[updater] Waiting for PID {} to exit...", args.pid);
    wait_for_pid(args.pid)?;
    eprintln!("[updater] PID {} exited", args.pid);

    // 2. Probe exclusive access on critical files
    eprintln!("[updater] Checking file locks...");
    probe_exclusive_access(&args.app_dir)?;

    let staging_dir = args.app_dir.join(".update-staging");
    let manifest_name = format!("manifest-{TARGET}.json");

    let manifest = if args.skip_download {
        // Pre-downloaded by main process - re-verify signature and validate version
        eprintln!("[updater] Using pre-downloaded staging...");
        let sig_name = format!("{manifest_name}.sig");

        let manifest_bytes =
            fs::read(staging_dir.join(&manifest_name)).context("read staged manifest")?;
        let sig_b64 =
            fs::read_to_string(staging_dir.join(&sig_name)).context("read staged signature")?;

        verify_manifest_signature(&manifest_bytes, &sig_b64)?;

        let manifest: Manifest =
            serde_json::from_slice(&manifest_bytes).context("parse staged manifest")?;

        if manifest.version != args.version {
            bail!(
                "staged manifest version mismatch: expected {}, got {}",
                args.version,
                manifest.version
            );
        }
        if manifest.target != TARGET {
            bail!(
                "manifest target mismatch: expected {TARGET}, got {}",
                manifest.target
            );
        }

        #[cfg(target_os = "linux")]
        enforce_sandbox_protocol_gate(&manifest)?;

        manifest
    } else {
        // Full download path (original behavior)
        eprintln!("[updater] Fetching release v{}...", args.version);
        let client = reqwest::blocking::Client::builder()
            .user_agent(format!("TidaLunar-Updater/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .context("failed to create HTTP client")?;

        let release = fetch_release(&client, &args.version)?;

        eprintln!("[updater] Downloading manifest...");
        let (manifest, _manifest_bytes) = download_and_verify_manifest(&client, &release)?;

        if manifest.target != TARGET {
            bail!(
                "manifest target mismatch: expected {TARGET}, got {}",
                manifest.target
            );
        }

        #[cfg(target_os = "linux")]
        enforce_sandbox_protocol_gate(&manifest)?;

        if staging_dir.exists() {
            fs::remove_dir_all(&staging_dir).context("failed to clean old staging dir")?;
        }
        fs::create_dir_all(&staging_dir).context("failed to create staging dir")?;

        eprintln!("[updater] Downloading update package...");
        // The currently-installed app version comes from its bundled manifest,
        // NOT env!("CARGO_PKG_VERSION") (which is the updater binary's own
        // version, unrelated to the app). delta_from is an app version.
        let installed_version = fs::read_to_string(args.app_dir.join("manifest.json"))
            .ok()
            .and_then(|s| serde_json::from_str::<Manifest>(&s).ok())
            .map(|m| m.version);
        let use_delta = matches!(
            (&manifest.delta_from, &installed_version),
            (Some(d), Some(i)) if d == i
        );
        let delta_name = delta_archive_name(&args.version);
        let (archive_name, archive_url) = {
            let delta = if use_delta {
                release.assets.iter().find(|a| a.name == delta_name)
            } else {
                None
            };
            match delta {
                Some(a) => {
                    eprintln!(
                        "[updater] Using delta from v{}",
                        installed_version.as_deref().unwrap_or("?")
                    );
                    (delta_name.clone(), a.browser_download_url.clone())
                }
                None => {
                    let full = format!("tidalunar_{}_{ARCHIVE_SUFFIX}", args.version);
                    let a = release
                        .assets
                        .iter()
                        .find(|x| x.name == full)
                        .context(format!("release missing asset: {full}"))?;
                    (full, a.browser_download_url.clone())
                }
            }
        };

        let archive_path = staging_dir.join(&archive_name);
        download_file(&client, &archive_url, &archive_path)?;

        eprintln!("[updater] Extracting...");
        extract_archive(&archive_path, &staging_dir)?;
        fs::remove_file(&archive_path).ok();

        manifest
    };

    // 7. Determine which files need updating
    eprintln!("[updater] Comparing files...");
    let mut files_to_update: Vec<(String, bool)> = Vec::new(); // (path, is_new)

    for (rel_path, entry) in &manifest.files {
        let local_path = args.app_dir.join(rel_path);
        let staged_path = staging_dir.join(rel_path);

        if !staged_path.exists() {
            eprintln!("[updater] Warning: manifest lists {rel_path} but not in zip, skipping");
            continue;
        }

        // Verify staged file matches manifest
        let staged_hash = sha256_file(&staged_path)?;
        if staged_hash != entry.sha256 {
            bail!(
                "staged file {rel_path} hash mismatch: expected {}, got {staged_hash}",
                entry.sha256
            );
        }

        // Check if local file differs
        let local_exists = local_path.exists();
        let needs_update = if local_exists {
            let local_hash = sha256_file(&local_path)?;
            local_hash != entry.sha256
        } else {
            true
        };

        if needs_update {
            files_to_update.push((rel_path.clone(), !local_exists));
        }
    }

    if files_to_update.is_empty() {
        eprintln!("[updater] All files already up to date");
        cleanup(&staging_dir, &args.app_dir, &[]);
        relaunch(&args.app_dir)?;
        return Ok(());
    }

    let file_names: Vec<&str> = files_to_update.iter().map(|(p, _)| p.as_str()).collect();
    eprintln!(
        "[updater] {} files to update: {}",
        files_to_update.len(),
        file_names.join(", ")
    );

    // 7b. Determine files to delete (present in old manifest but absent in new)
    let deleted_files: Vec<String> = cleanup::read_manifest(&args.app_dir.join("manifest.json"))
        .map(|old_manifest| cleanup::diff_removed(&old_manifest, &manifest, &args.app_dir))
        .unwrap_or_default();
    if !deleted_files.is_empty() {
        eprintln!(
            "[updater] {} files to remove: {}",
            deleted_files.len(),
            deleted_files.join(", ")
        );
    }

    // 8. Write transaction journal
    let journal_path = args.app_dir.join(".update-journal.json");
    let journal = Journal {
        version: args.version.clone(),
        state: JournalState::Pending,
        files: files_to_update
            .iter()
            .map(|(p, is_new)| JournalFile {
                path: p.clone(),
                backup: format!("{p}.bak"),
                is_new: *is_new,
            })
            .collect(),
        deleted_files,
    };
    write_journal(&journal_path, &journal)?;

    // 9. Commit phase - rename originals to .bak, move staged to final
    eprintln!("[updater] Applying update...");
    for jf in &journal.files {
        let original = args.app_dir.join(&jf.path);
        let backup = args.app_dir.join(&jf.backup);
        let staged = staging_dir.join(&jf.path);

        // Ensure parent dir exists for new files
        if let Some(parent) = original.parent() {
            fs::create_dir_all(parent).ok();
        }

        // Rename original → .bak (if exists)
        if original.exists() {
            if backup.exists() {
                fs::remove_file(&backup).ok();
            }
            fs::rename(&original, &backup)
                .with_context(|| format!("failed to backup {}", jf.path))?;
        }

        // Move staged → final position
        if let Err(e) = fs::rename(&staged, &original) {
            // rename failed - try copy as fallback (cross-device)
            fs::copy(&staged, &original)
                .with_context(|| format!("failed to install {} (rename: {e})", jf.path))?;
            fs::remove_file(&staged).ok();
        }
    }

    // Delete obsolete files (from old layout)
    for del_path in &journal.deleted_files {
        let to_delete = args.app_dir.join(del_path);
        if fs::remove_file(&to_delete).is_ok() {
            eprintln!("[updater] Removed obsolete: {del_path}");
        }
    }

    // Mark journal as committed
    let committed = Journal {
        version: journal.version,
        state: JournalState::Committed,
        files: journal.files,
        deleted_files: journal.deleted_files,
    };
    write_journal(&journal_path, &committed)?;

    // 10. Cleanup and relaunch
    eprintln!("[updater] Cleaning up...");
    cleanup(&staging_dir, &args.app_dir, &committed.files);

    eprintln!("[updater] Update complete, relaunching...");
    relaunch(&args.app_dir)?;

    Ok(())
}

// ---------------------------------------------------------------------------
// PID waiting
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
fn wait_for_pid(pid: u32) -> Result<()> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };

    const WAIT_OBJECT_0: u32 = 0x00000000;
    const WAIT_TIMEOUT: u32 = 0x00000102;

    unsafe {
        let handle = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
        if handle.is_null() {
            // Process already gone
            return Ok(());
        }
        // Wait up to 30 seconds
        let result = WaitForSingleObject(handle, 30_000);
        CloseHandle(handle);
        match result {
            WAIT_OBJECT_0 => Ok(()),
            WAIT_TIMEOUT => bail!("timeout waiting for PID {pid} to exit"),
            _ => bail!("WaitForSingleObject failed for PID {pid} (code {result:#010x})"),
        }
    }
}

#[cfg(not(target_os = "windows"))]
fn wait_for_pid(pid: u32) -> Result<()> {
    use std::ffi::c_int;
    unsafe extern "C" {
        fn kill(pid: c_int, sig: c_int) -> c_int;
    }

    for _ in 0..60 {
        // signal 0 = check if process exists
        if unsafe { kill(pid as c_int, 0) } != 0 {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(500));
    }
    bail!("timeout waiting for PID {pid} to exit");
}

// ---------------------------------------------------------------------------
// Path safety
// ---------------------------------------------------------------------------

/// Reject absolute paths and directory-escape components (e.g. "..", prefix).
/// Returns true only if `app_dir.join(rel)` resolves to a path under `app_dir`.
pub(crate) fn is_safe_relative_path(rel: &str, app_dir: &Path) -> bool {
    let p = Path::new(rel);
    if p.is_absolute() {
        return false;
    }
    for component in p.components() {
        match component {
            std::path::Component::ParentDir | std::path::Component::Prefix(_) => return false,
            _ => {}
        }
    }
    // Final check: canonicalize-free containment
    app_dir.join(rel).starts_with(app_dir)
}

// ---------------------------------------------------------------------------
// File lock probing
// ---------------------------------------------------------------------------

fn probe_exclusive_access(app_dir: &Path) -> Result<()> {
    // Check the exe at root
    let exe_name = if cfg!(target_os = "windows") {
        "tidalunar.exe"
    } else {
        "tidalunar"
    };
    let mut paths_to_check: Vec<std::path::PathBuf> = vec![app_dir.join(exe_name)];

    // Check CEF DLLs in both old (root) and new (bin/cef/) layouts
    let cef_libs: &[&str] = if cfg!(target_os = "windows") {
        &[
            "libcef.dll",
            "chrome_elf.dll",
            "libEGL.dll",
            "libGLESv2.dll",
        ]
    } else {
        &["libcef.so"]
    };
    for lib in cef_libs {
        for subdir in &["", "cef", "bin/cef"] {
            paths_to_check.push(app_dir.join(subdir).join(lib));
        }
    }

    // Check bun in both old (root) and new (bin/) layouts
    let bun_name = if cfg!(target_os = "windows") {
        "bun.exe"
    } else {
        "bun"
    };
    paths_to_check.push(app_dir.join(bun_name));
    paths_to_check.push(app_dir.join("bin").join(bun_name));

    for path in &paths_to_check {
        if !path.exists() {
            continue;
        }

        let mut locked = true;
        for attempt in 1..=3 {
            if try_exclusive_access(path)? {
                locked = false;
                break;
            }
            let display = path.file_name().unwrap_or_default().to_string_lossy();
            eprintln!("[updater] {display} is locked, retry {attempt}/3...");
            thread::sleep(Duration::from_secs(2));
        }

        if locked {
            let display = path.file_name().unwrap_or_default().to_string_lossy();
            bail!("{display} is still locked by another process - cannot update");
        }
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn try_exclusive_access(path: &Path) -> Result<bool> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_WRITE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, OPEN_EXISTING,
    };

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_WRITE,
            0, // no sharing
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };

    if handle == INVALID_HANDLE_VALUE {
        Ok(false) // locked
    } else {
        unsafe { CloseHandle(handle) };
        Ok(true) // accessible
    }
}

#[cfg(not(target_os = "windows"))]
fn try_exclusive_access(path: &Path) -> Result<bool> {
    use std::os::unix::io::AsRawFd;

    let file = match fs::OpenOptions::new().write(true).open(path) {
        Ok(f) => f,
        Err(_) => return Ok(false),
    };

    unsafe extern "C" {
        fn flock(fd: i32, operation: i32) -> i32;
    }

    const LOCK_EX: i32 = 2;
    const LOCK_NB: i32 = 4;
    const LOCK_UN: i32 = 8;

    let fd = file.as_raw_fd();
    let result = unsafe { flock(fd, LOCK_EX | LOCK_NB) };
    if result == 0 {
        // Got the lock - unlock and report accessible
        unsafe { flock(fd, LOCK_UN) };
        Ok(true)
    } else {
        Ok(false)
    }
}

// ---------------------------------------------------------------------------
// GitHub API
// ---------------------------------------------------------------------------

fn fetch_release(client: &reqwest::blocking::Client, version: &str) -> Result<GhRelease> {
    let url = format!(
        "https://api.github.com/repos/{GITHUB_OWNER}/{GITHUB_REPO}/releases/tags/{version}"
    );
    let resp = client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .context("failed to fetch release")?;

    if !resp.status().is_success() {
        bail!(
            "GitHub API returned {}: {}",
            resp.status(),
            resp.text().unwrap_or_default()
        );
    }

    resp.json::<GhRelease>()
        .context("failed to parse release JSON")
}

/// Verify an Ed25519 signature over manifest bytes.
fn verify_manifest_signature(manifest_bytes: &[u8], sig_b64: &str) -> Result<()> {
    let sig_bytes = BASE64
        .decode(sig_b64.trim())
        .context("invalid base64 in signature")?;
    let signature =
        Signature::from_slice(&sig_bytes).context("invalid Ed25519 signature format")?;
    let verifying_key =
        VerifyingKey::from_bytes(&UPDATE_PUBLIC_KEY).context("invalid embedded public key")?;
    verifying_key
        .verify(manifest_bytes, &signature)
        .context("manifest signature verification FAILED - update rejected")?;
    eprintln!("[updater] Manifest signature verified");
    Ok(())
}

fn download_and_verify_manifest(
    client: &reqwest::blocking::Client,
    release: &GhRelease,
) -> Result<(Manifest, Vec<u8>)> {
    let manifest_name = format!("manifest-{TARGET}.json");
    let sig_name = format!("manifest-{TARGET}.json.sig");

    let manifest_url = release
        .assets
        .iter()
        .find(|a| a.name == manifest_name)
        .context(format!("release missing {manifest_name}"))?
        .browser_download_url
        .clone();

    let sig_url = release
        .assets
        .iter()
        .find(|a| a.name == sig_name)
        .context(format!("release missing {sig_name}"))?
        .browser_download_url
        .clone();

    // Download manifest
    let manifest_bytes = client
        .get(&manifest_url)
        .send()
        .context("download manifest")?
        .bytes()
        .context("read manifest bytes")?
        .to_vec();

    // Download signature
    let sig_b64 = client
        .get(&sig_url)
        .send()
        .context("download signature")?
        .text()
        .context("read signature")?;

    verify_manifest_signature(&manifest_bytes, &sig_b64)?;

    // Parse manifest
    let manifest: Manifest =
        serde_json::from_slice(&manifest_bytes).context("invalid manifest JSON")?;

    Ok((manifest, manifest_bytes))
}

// ---------------------------------------------------------------------------
// Download + extract
// ---------------------------------------------------------------------------

fn download_file(client: &reqwest::blocking::Client, url: &str, dest: &Path) -> Result<()> {
    let resp = client.get(url).send().context("download failed")?;
    if !resp.status().is_success() {
        bail!("download returned {}", resp.status());
    }

    let bytes = resp.bytes().context("read download body")?;
    fs::write(dest, &bytes).with_context(|| format!("write to {}", dest.display()))?;
    Ok(())
}

fn extract_archive(archive_path: &Path, dest_dir: &Path) -> Result<()> {
    #[cfg(target_os = "windows")]
    return extract_zip(archive_path, dest_dir);
    #[cfg(target_os = "linux")]
    return extract_tar_gz(archive_path, dest_dir);
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        let _ = (archive_path, dest_dir);
        anyhow::bail!("update extraction unsupported on this platform");
    }
}

#[cfg(target_os = "windows")]
fn extract_zip(zip_path: &Path, dest_dir: &Path) -> Result<()> {
    let file = fs::File::open(zip_path).context("open zip")?;
    let mut archive = zip::ZipArchive::new(file).context("parse zip")?;

    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).context("zip entry")?;
        let name = entry.name().to_string();

        if !is_safe_relative_path(&name, dest_dir) {
            anyhow::bail!("zip entry has unsafe path: {name}");
        }

        // Skip directories
        if entry.is_dir() {
            let dir_path = dest_dir.join(&name);
            fs::create_dir_all(&dir_path).ok();
            continue;
        }

        let out_path = dest_dir.join(&name);
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
/// directory, stripping it so files land at `dest_dir` root to match the
/// manifest's relative paths. Unix modes from the tar header are preserved.
#[cfg(target_os = "linux")]
fn extract_tar_gz(archive_path: &Path, dest_dir: &Path) -> Result<()> {
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
        if !is_safe_relative_path(&rel_str, dest_dir) {
            anyhow::bail!("tar entry has unsafe path: {}", path.display());
        }

        let out_path = dest_dir.join(rel);
        let etype = entry.header().entry_type();
        if etype.is_dir() {
            fs::create_dir_all(&out_path).ok();
            continue;
        }
        if !etype.is_file() {
            // Plain files only; reject symlinks/hardlinks/devices (traversal
            // vector in an archive not hash-bound before extraction).
            anyhow::bail!("tar entry {rel_str} has disallowed type {etype:?}");
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

// ---------------------------------------------------------------------------
// Hashing
// ---------------------------------------------------------------------------

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
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

// ---------------------------------------------------------------------------
// Journal
// ---------------------------------------------------------------------------

fn write_journal(path: &Path, journal: &Journal) -> Result<()> {
    let json = serde_json::to_string_pretty(journal).context("serialize journal")?;
    let mut file =
        fs::File::create(path).with_context(|| format!("create journal {}", path.display()))?;
    file.write_all(json.as_bytes())?;
    file.sync_all()?; // fsync
    Ok(())
}

/// Recover from an interrupted update.
/// - "pending" journal → rollback: restore .bak files, delete staging + journal
/// - "committed" journal → cleanup: delete .bak files, delete staging + journal
#[allow(dead_code)]
fn recover_journal(app_dir: &Path) -> Result<bool> {
    let journal_path = app_dir.join(".update-journal.json");
    if !journal_path.exists() {
        return Ok(false);
    }

    let data = fs::read_to_string(&journal_path).context("read journal")?;
    let journal: Journal = serde_json::from_str(&data).context("parse journal")?;

    match journal.state {
        JournalState::Pending => {
            // Rollback: restore .bak → original, remove new files
            eprintln!("[updater] Recovery: rolling back incomplete update");
            for jf in &journal.files {
                let original = app_dir.join(&jf.path);
                let backup = app_dir.join(&jf.backup);
                if jf.is_new {
                    // No original existed - remove the newly installed file
                    fs::remove_file(&original).ok();
                } else if backup.exists() {
                    if original.exists() {
                        fs::remove_file(&original).ok();
                    }
                    fs::rename(&backup, &original).ok();
                }
            }
        }
        JournalState::Committed => {
            // Cleanup: remove .bak files
            eprintln!("[updater] Recovery: cleaning up completed update");
            for jf in &journal.files {
                let backup = app_dir.join(&jf.backup);
                fs::remove_file(&backup).ok();
            }
        }
    }

    // Clean up staging and journal
    let staging = app_dir.join(".update-staging");
    if staging.exists() {
        fs::remove_dir_all(&staging).ok();
    }
    fs::remove_file(&journal_path).ok();

    Ok(true)
}

// ---------------------------------------------------------------------------
// Cleanup and relaunch
// ---------------------------------------------------------------------------

fn cleanup(staging_dir: &Path, app_dir: &Path, files: &[JournalFile]) {
    // Remove .bak files
    for jf in files {
        let backup = app_dir.join(&jf.backup);
        fs::remove_file(&backup).ok();
    }

    // Remove staging dir
    if staging_dir.exists() {
        fs::remove_dir_all(staging_dir).ok();
    }

    // Remove journal
    let journal_path = app_dir.join(".update-journal.json");
    fs::remove_file(&journal_path).ok();
}

fn relaunch(app_dir: &Path) -> Result<()> {
    let exe = app_dir.join(EXE_NAME);

    let mut cmd = Command::new(&exe);
    cmd.current_dir(app_dir);

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW - app will create its own
    }

    cmd.spawn()
        .with_context(|| format!("failed to relaunch {}", exe.display()))?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Error display
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
fn show_error(msg: &str) {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;

    let wide_msg: Vec<u16> = OsStr::new(msg).encode_wide().chain(Some(0)).collect();
    let wide_title: Vec<u16> = OsStr::new("TidaLunar Update Error")
        .encode_wide()
        .chain(Some(0))
        .collect();

    unsafe {
        windows_sys::Win32::UI::WindowsAndMessaging::MessageBoxW(
            std::ptr::null_mut(),
            wide_msg.as_ptr(),
            wide_title.as_ptr(),
            windows_sys::Win32::UI::WindowsAndMessaging::MB_OK
                | windows_sys::Win32::UI::WindowsAndMessaging::MB_ICONERROR,
        );
    }
}

#[cfg(not(target_os = "windows"))]
fn show_error(msg: &str) {
    eprintln!("ERROR: {msg}");
}

#[cfg(test)]
mod manifest_tests {
    use super::*;

    #[test]
    fn manifest_roundtrip_with_protocol_field() {
        let json = r#"{
            "version": "0.0.5-alpha",
            "min_version": "0.0.4-alpha",
            "target": "linux-amd64",
            "files": {},
            "sandbox_protocol_required": 1
        }"#;
        let m: Manifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.sandbox_protocol_required, Some(1));
        let serialized = serde_json::to_string(&m).unwrap();
        assert!(serialized.contains("\"sandbox_protocol_required\":1"));
    }

    #[test]
    fn manifest_roundtrip_without_protocol_field_defaults_none() {
        let json = r#"{
            "version": "0.0.4-alpha",
            "min_version": "0.0.4-alpha",
            "target": "windows-amd64",
            "files": {}
        }"#;
        let m: Manifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.sandbox_protocol_required, None);
    }

    #[test]
    fn manifest_delta_from_roundtrip() {
        let json = r#"{"version":"0.0.5-alpha","min_version":"0.0.4-alpha","target":"linux-amd64","files":{},"delta_from":"0.0.4-alpha"}"#;
        let m: Manifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.delta_from.as_deref(), Some("0.0.4-alpha"));
    }
}
