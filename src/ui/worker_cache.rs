// FRAGILE: depends on Chromium's profile layout (`Service Worker/` holding the worker's cache,
// registrations and scripts). Revalidate on CEF upgrades. Should it move, the drop misses and
// `cache_heal.js`, which compares the same localStorage epoch once a page runs, is what remains.
//
// A page TIDAL's worker controls runs chunks straight from its cache, past every filter, rewritten
// by an older binary. Dropping the worker before CEF starts lets the first page fetch everything
// through the filters; the worker it registers precaches the new shapes.

use std::io::ErrorKind;
use std::path::Path;

use crate::platform::sdk_storage::{self, LsValue};
use crate::ui::module_capture::{REWRITE_EPOCH, epoch_text};

/// Also handed to `cache_heal.js` (prologue.rs), which records it when a page fills a cache.
pub(crate) const EPOCH_KEY: &str = "__luna_rewrite_epoch";

const WORKER_DIR: &str = "Service Worker";
/// A name Chromium never reads. Swept at every launch, since a leftover blocks the next rename.
const WORKER_TRASH: &str = "Service Worker.stale";

/// What the profile's worker cache holds against this binary's rewrites.
#[derive(Debug, PartialEq, Eq)]
enum WorkerCache {
    Current,
    /// Filled under other rewrites, or under no recorded epoch at all.
    Stale,
    /// Local Storage would not open: another process holds the profile (its files stay).
    Held,
}

impl WorkerCache {
    fn judge(stored: &LsValue, current: &str) -> Self {
        match stored {
            LsValue::Unreadable => Self::Held,
            LsValue::Value(bytes) if bytes.as_slice() == current.as_bytes() => Self::Current,
            LsValue::Value(_) | LsValue::Missing | LsValue::Corrupt => Self::Stale,
        }
    }
}

/// Before `initialize()`, while no Chromium process can hold these files.
pub(crate) fn drop_if_stale(profile: &Path) {
    let trash = profile.join(WORKER_TRASH);
    if let Err(e) = remove_if_present(&trash) {
        crate::verr!("[HEAL]   retired worker storage not swept ({e}); the next launch retries");
    }
    let leveldb = profile.join("Local Storage").join("leveldb");
    let current = epoch_text(REWRITE_EPOCH);
    match WorkerCache::judge(&sdk_storage::read_ls_value(&leveldb, EPOCH_KEY), &current) {
        WorkerCache::Current => {}
        WorkerCache::Held => {
            crate::verr!(
                "[HEAL]   Local Storage unreadable (locked?); the worker cache is left to the page"
            )
        }
        WorkerCache::Stale => {
            if let Err(e) = retire_worker(profile, &trash) {
                crate::verr!("[HEAL]   worker cache not dropped ({e}); the page heals it instead");
                return;
            }
            if sdk_storage::write_ls_values(&leveldb, &[(EPOCH_KEY, current.as_bytes())]).is_none()
            {
                crate::verr!(
                    "[HEAL]   epoch {current} not recorded; the next launch drops the cache again"
                );
                return;
            }
            if let Err(e) = remove_if_present(&trash) {
                crate::verr!(
                    "[HEAL]   retired worker storage left in place ({e}); swept next launch"
                );
            }
            crate::vprintln!("[HEAL]   worker cache dropped, epoch {current} recorded");
        }
    }
}

/// One rename: Chromium finds the whole old storage or none of it, never a subset of its stores.
fn retire_worker(profile: &Path, trash: &Path) -> std::io::Result<()> {
    match std::fs::rename(profile.join(WORKER_DIR), trash) {
        Err(e) if e.kind() != ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

fn remove_if_present(dir: &Path) -> std::io::Result<()> {
    match std::fs::remove_dir_all(dir) {
        Err(e) if e.kind() != ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

#[cfg(test)]
#[path = "../../tests/unit/ui/worker_cache.rs"]
mod tests;
