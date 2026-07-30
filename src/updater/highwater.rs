//! Anti-replay / anti-downgrade high-water mark.
//!
//! Persists the highest app version ever launched; the updater rejects any
//! manifest whose target is `<=` it, even with a valid signature - the
//! freshness check a signature can't give, since an old validly-signed
//! manifest can be replayed as a downgrade.
//!
//! Guards only against remote replay (stale CDN, MITM, mirror), not a local
//! attacker who could rewrite the mark - so a plain atomic 0600 file suffices.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const MARK_FILE: &str = ".update-mark.json";

#[derive(serde::Serialize, serde::Deserialize)]
struct Mark {
    max_version: String,
    recorded_at: u64,
}

fn mark_path(data_dir: &Path) -> PathBuf {
    data_dir.join(MARK_FILE)
}

/// Highest app version ever launched, or `"0.0.0"` if missing/corrupt
/// (fail-open: a bad mark disables the gate, it does not block updates).
pub(super) fn load(data_dir: &Path) -> String {
    match std::fs::read(mark_path(data_dir)) {
        Ok(bytes) => serde_json::from_slice::<Mark>(&bytes)
            .ok()
            .filter(|m| semver::Version::parse(&m.max_version).is_ok())
            .map(|m| m.max_version)
            .unwrap_or_else(|| "0.0.0".to_string()),
        Err(_) => "0.0.0".to_string(),
    }
}

/// Record `current` if it exceeds the stored mark. Monotonic (never lowers);
/// best-effort, logging on I/O failure.
pub(super) fn record(data_dir: &Path, current: &str) {
    let existing = load(data_dir);
    if !super::util::is_newer(current, &existing) {
        return; // current <= stored; keep the higher mark
    }
    let recorded_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mark = Mark {
        max_version: current.to_string(),
        recorded_at,
    };
    let Ok(bytes) = serde_json::to_vec_pretty(&mark) else {
        return;
    };
    if let Err(e) = atomic_write(data_dir, &mark_path(data_dir), &bytes) {
        crate::vprintln!("[UPDATER] failed to record high-water mark: {e}");
    } else {
        crate::vprintln!("[UPDATER] high-water mark = v{current}");
    }
}

fn atomic_write(dir: &Path, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tmp.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

#[cfg(test)]
#[path = "../../tests/unit/updater/highwater.rs"]
mod tests;
