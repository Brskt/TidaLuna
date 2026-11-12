// Every line goes through say!, which a dead stderr cannot turn into a panic.
#![deny(clippy::print_stderr, clippy::print_stdout)]
#![deny(clippy::allow_attributes)]
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// `eprintln!` without its panic. The app spawns the updater on its own stderr, and once that
/// pipe or terminal is gone (EPIPE, ENOSPC) `eprintln!` would panic mid-install. A line that
/// cannot be shown is dropped, as the app's own log does.
macro_rules! say {
    ($($arg:tt)*) => {{
        let _ = writeln!(std::io::stderr(), $($arg)*);
    }};
}

mod cleanup;
mod cli;
mod download;
mod extract;
#[cfg(target_os = "windows")]
mod lock;
mod manifest;
mod platform;
mod process;

#[path = "../../src/updater/repo.rs"]
mod repo;

use cli::{parse_args, run_cleanup_stale};
use download::{download_and_verify_manifest, download_file, fetch_release};
use extract::extract_archive;
use manifest::{is_safe_relative_path, sha256_file, verify_delta_base, verify_manifest_signature};
#[cfg(target_os = "linux")]
use platform::enforce_sandbox_protocol_gate;
use platform::{ARCHIVE_SUFFIX, TARGET, delta_archive_name};
use process::{probe_exclusive_access, relaunch, show_error, wait_for_pid};

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
    assets: Vec<GhAsset>,
}

#[derive(Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
}

struct Args {
    pid: u32,
    version: String,
    app_dir: PathBuf,
    skip_download: bool,
}

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
        say!("Update failed: {e:#}");
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

    // Cross-process install/update mutex, in a namespace bounded by our SID; serialises
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
    say!("[updater] Waiting for PID {} to exit...", args.pid);
    wait_for_pid(args.pid)?;
    say!("[updater] PID {} exited", args.pid);

    // 2. Probe exclusive access on critical files
    say!("[updater] Checking file locks...");
    probe_exclusive_access(&args.app_dir)?;

    let staging_dir = args.app_dir.join(".update-staging");

    let manifest = if args.skip_download {
        verify_prestaged(&args, &staging_dir)?
    } else {
        download_and_stage(&args, &staging_dir)?
    };

    // 7. Determine which files need updating
    let files_to_update = plan_updates(&args, &manifest, &staging_dir)?;

    if files_to_update.is_empty() {
        say!("[updater] All files already up to date");
        cleanup(&staging_dir, &args.app_dir, &[]);
        relaunch(&args.app_dir)?;
        return Ok(());
    }

    let file_names: Vec<&str> = files_to_update.iter().map(|(p, _)| p.as_str()).collect();
    say!(
        "[updater] {} files to update: {}",
        files_to_update.len(),
        file_names.join(", ")
    );

    // 7b. Determine files to delete (present in old manifest but absent in new)
    let deleted_files: Vec<String> = cleanup::read_manifest(&args.app_dir.join("manifest.json"))
        .map(|old_manifest| cleanup::diff_removed(&old_manifest, &manifest, &args.app_dir))
        .unwrap_or_default();
    if !deleted_files.is_empty() {
        say!(
            "[updater] {} files to remove: {}",
            deleted_files.len(),
            deleted_files.join(", ")
        );
    }

    let committed = commit_update(&args, &staging_dir, &files_to_update, deleted_files)?;

    // 10. Cleanup and relaunch
    say!("[updater] Cleaning up...");
    cleanup(&staging_dir, &args.app_dir, &committed.files);

    say!("[updater] Update complete, relaunching...");
    relaunch(&args.app_dir)?;

    Ok(())
}

fn verify_prestaged(args: &Args, staging_dir: &Path) -> Result<Manifest> {
    // Pre-downloaded by main process - re-verify signature and validate version
    say!("[updater] Using pre-downloaded staging...");
    let manifest_name = format!("manifest-{TARGET}.json");
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

    // This gate turns "unchanged" from an inference about versions into a verified claim. Only
    // the `--skip-download` path needs it, the archive having been staged by the app with no
    // loop of ours to check it. download_and_stage already ran the same call inside its loop,
    // against the same manifest and directories with nothing touched in between; a
    // second call there re-hashes the whole install for an answer that cannot have changed.
    if let Err(what) = verify_delta_base(&manifest, &args.app_dir, staging_dir) {
        bail!("{what}");
    }

    Ok(manifest)
}

fn download_and_stage(args: &Args, staging_dir: &Path) -> Result<Manifest> {
    // Full download path (original behavior)
    say!("[updater] Fetching release v{}...", args.version);
    let client = reqwest::blocking::Client::builder()
        .user_agent(format!("TidaLunar-Updater/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .context("failed to create HTTP client")?;

    let release = fetch_release(&client, &args.version)?;

    say!("[updater] Downloading manifest...");
    let manifest = download_and_verify_manifest(&client, &release)?;

    if manifest.target != TARGET {
        bail!(
            "manifest target mismatch: expected {TARGET}, got {}",
            manifest.target
        );
    }

    #[cfg(target_os = "linux")]
    enforce_sandbox_protocol_gate(&manifest)?;

    // The currently-installed app version comes from its bundled manifest,
    // NOT env!("CARGO_PKG_VERSION") (which is the updater binary's own
    // version, unrelated to the app). delta_from is an app version.
    let installed_version =
        cleanup::read_manifest(&args.app_dir.join("manifest.json")).map(|m| m.version);
    let mut use_delta = matches!(
        (&manifest.delta_from, &installed_version),
        (Some(d), Some(i)) if d == i
    );
    // A delta assumes every file it left out is already correct on disk, a claim about the
    // two releases and not about this install. Checked after extraction, with the full
    // archive retried when it fails; bailing would let one damaged file block every future
    // update through this entry point, as the in-app downloader also reasons.
    loop {
        if staging_dir.exists() {
            fs::remove_dir_all(staging_dir).context("failed to clean old staging dir")?;
        }
        fs::create_dir_all(staging_dir).context("failed to create staging dir")?;

        say!("[updater] Downloading update package...");
        let delta_name = delta_archive_name(&args.version);
        let (archive_name, archive_url) = {
            let delta = if use_delta {
                release.assets.iter().find(|a| a.name == delta_name)
            } else {
                None
            };
            match delta {
                Some(a) => {
                    say!(
                        "[updater] Using delta from v{}",
                        installed_version.as_deref().unwrap_or("?")
                    );
                    (delta_name.clone(), a.browser_download_url.clone())
                }
                None => {
                    use_delta = false;
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

        say!("[updater] Extracting...");
        extract_archive(&archive_path, staging_dir)?;
        fs::remove_file(&archive_path).ok();

        match verify_delta_base(&manifest, &args.app_dir, staging_dir) {
            Ok(()) => break,
            // Only reachable while `use_delta` holds, and the retry clears it; the
            // fallback fires exactly once.
            Err(what) if use_delta => {
                say!(
                    "[updater] Local base does not match the delta's assumption ({what}); \
                     re-downloading the full archive"
                );
                use_delta = false;
            }
            Err(what) => bail!("{what}"),
        }
    }

    Ok(manifest)
}

fn plan_updates(
    args: &Args,
    manifest: &Manifest,
    staging_dir: &Path,
) -> Result<Vec<(String, bool)>> {
    say!("[updater] Comparing files...");
    let mut files_to_update: Vec<(String, bool)> = Vec::new(); // (path, is_new)

    for (rel_path, entry) in &manifest.files {
        let local_path = args.app_dir.join(rel_path);
        let staged_path = staging_dir.join(rel_path);

        if !staged_path.exists() {
            // Absent from the archive is the release calling this file unchanged, a claim
            // `verify_delta_base` has already checked against this disk on either path (in
            // download_and_stage's loop, or at the end of verify_prestaged). The existing
            // copy stands.
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

    Ok(files_to_update)
}

fn commit_update(
    args: &Args,
    staging_dir: &Path,
    files_to_update: &[(String, bool)],
    deleted_files: Vec<String>,
) -> Result<Journal> {
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
    say!("[updater] Applying update...");
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
            say!("[updater] Removed obsolete: {del_path}");
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

    Ok(committed)
}

fn write_journal(path: &Path, journal: &Journal) -> Result<()> {
    let json = serde_json::to_string_pretty(journal).context("serialize journal")?;
    let mut file =
        fs::File::create(path).with_context(|| format!("create journal {}", path.display()))?;
    file.write_all(json.as_bytes())?;
    file.sync_all()?; // fsync
    Ok(())
}

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

#[cfg(test)]
#[path = "../tests/unit/main/main_tests.rs"]
mod main_tests;
