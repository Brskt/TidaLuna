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

// ---------------------------------------------------------------------------
// Types
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
    state: JournalState,
    files: Vec<JournalFile>,
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

fn run() -> Result<()> {
    let args = parse_args()?;

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
        // Pre-downloaded by main process — re-verify signature and validate version
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

        if staging_dir.exists() {
            fs::remove_dir_all(&staging_dir).context("failed to clean old staging dir")?;
        }
        fs::create_dir_all(&staging_dir).context("failed to create staging dir")?;

        eprintln!("[updater] Downloading update package...");
        let zip_asset_name = format!("tidalunar-{TARGET}.zip");
        let zip_url = release
            .assets
            .iter()
            .find(|a| a.name == zip_asset_name)
            .context(format!("release missing asset: {zip_asset_name}"))?
            .browser_download_url
            .clone();

        let zip_path = staging_dir.join("update.zip");
        download_file(&client, &zip_url, &zip_path)?;

        eprintln!("[updater] Extracting...");
        extract_zip(&zip_path, &staging_dir)?;
        fs::remove_file(&zip_path).ok();

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
    };
    write_journal(&journal_path, &journal)?;

    // 9. Commit phase — rename originals to .bak, move staged to final
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
            // rename failed — try copy as fallback (cross-device)
            fs::copy(&staged, &original)
                .with_context(|| format!("failed to install {} (rename: {e})", jf.path))?;
            fs::remove_file(&staged).ok();
        }
    }

    // Mark journal as committed
    let committed = Journal {
        version: journal.version,
        state: JournalState::Committed,
        files: journal.files,
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
// File lock probing
// ---------------------------------------------------------------------------

fn probe_exclusive_access(app_dir: &Path) -> Result<()> {
    let critical_files: &[&str] = if cfg!(target_os = "windows") {
        &["tidalunar.exe", "libcef.dll", "bun.exe"]
    } else {
        &["tidalunar", "libcef.so", "bun"]
    };

    for name in critical_files {
        let path = app_dir.join(name);
        if !path.exists() {
            continue;
        }

        let mut locked = true;
        for attempt in 1..=3 {
            if try_exclusive_access(&path)? {
                locked = false;
                break;
            }
            eprintln!("[updater] {name} is locked, retry {attempt}/3...");
            thread::sleep(Duration::from_secs(2));
        }

        if locked {
            bail!("{name} is still locked by another process — cannot update");
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
        // Got the lock — unlock and report accessible
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
        .context("manifest signature verification FAILED — update rejected")?;
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

fn extract_zip(zip_path: &Path, dest_dir: &Path) -> Result<()> {
    let file = fs::File::open(zip_path).context("open zip")?;
    let mut archive = zip::ZipArchive::new(file).context("parse zip")?;

    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).context("zip entry")?;
        let name = entry.name().to_string();

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

        // Preserve executable permission on Unix
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
    Ok(format!("{:x}", hasher.finalize()))
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
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW — app will create its own
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
