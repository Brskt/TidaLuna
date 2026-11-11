use rusqlite::Connection;
use rusqlite::backup::{Backup, StepResult};
use std::path::{Path, PathBuf};

pub(super) fn beside(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// Copies of damaged stores kept beside the live one; older ones are dropped first.
pub(super) const KEPT_ASIDES: usize = 3;

/// A name no file holds, with its two companions free too: a later incident never lands on an
/// earlier one's file, nor on a `-wal` SQLite would replay into it.
pub(crate) fn aside_name(path: &Path) -> PathBuf {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    (0u32..)
        .map(|n| beside(path, &format!(".corrupt-{ms}-{n}")))
        .find(|aside| {
            ["", "-wal", "-shm"]
                .iter()
                .all(|suffix| !beside(aside, suffix).exists())
        })
        .expect("an unused name exists")
}

/// Drops all but the newest `KEPT_ASIDES` copies, each with its companions.
fn prune_asides(path: &Path) {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
        return;
    };
    let prefix = format!("{}.corrupt-", name.to_string_lossy());
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut asides: Vec<(u128, u32, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let file_name = entry.file_name().to_string_lossy().into_owned();
            let (ms, n) = file_name.strip_prefix(&prefix)?.split_once('-')?;
            Some((ms.parse().ok()?, n.parse().ok()?, entry.path()))
        })
        .collect();
    asides.sort();
    let surplus = asides.len().saturating_sub(KEPT_ASIDES);
    for (_, _, aside) in asides.into_iter().take(surplus) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(beside(&aside, suffix));
        }
    }
}

/// Copies `from` into a new file at `to`, in one SQLite transaction on the destination. On unix the
/// online backup opens the file by the bytes of its path, as `Connection::open` does; `VACUUM INTO`
/// takes its filename as SQL text, which a path that is not UTF-8 cannot be. The destination
/// connection is closed when this returns (Windows renames only a file no handle is open on).
fn write_whole(from: &Connection, to: &Path) -> Result<(), String> {
    let mut dest = Connection::open(to).map_err(|e| e.to_string())?;
    let backup = Backup::new(from, &mut dest).map_err(|e| e.to_string())?;
    match backup.step(-1) {
        Ok(StepResult::Done) => Ok(()),
        Ok(other) => Err(format!("backup stopped at {other:?}")),
        Err(e) => Err(e.to_string()),
    }
}

/// Puts the store held in `fresh` at `path`. It is written whole beside the damaged file first and
/// renamed over it last; at every instant `path` holds the old store or the complete new one:
/// the damaged file is copied aside, never moved. The caller has dropped its connection to `path`
/// (Windows renames only a file no handle is open on: SQLite opens without `FILE_SHARE_DELETE`).
pub(crate) fn replace(path: &Path, fresh: &Connection) -> Result<PathBuf, String> {
    let aside = aside_name(path);
    replace_naming(path, fresh, &aside)?;
    Ok(aside)
}

/// `replace` with the copy's name chosen by the caller, for a store that has to carry that name
/// inside itself. What `fresh` holds is renamed into place whole; a fact written into it before
/// the call is there exactly when the replacement is.
pub(crate) fn replace_naming(path: &Path, fresh: &Connection, aside: &Path) -> Result<(), String> {
    let new = beside(path, ".new");
    for suffix in ["", "-wal", "-shm", "-journal"] {
        match std::fs::remove_file(beside(&new, suffix)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                return Err(format!("clear {}: {e}", beside(&new, suffix).display()));
            }
            _ => {}
        }
    }
    if let Err(e) = write_whole(fresh, &new) {
        // A half-written file the next launch would only clear.
        let _ = std::fs::remove_file(&new);
        return Err(format!("write {}: {e}", new.display()));
    }
    // A `-wal` at the target would pair with the new file: it is only ever empty or absent once
    // the old connection is closed, unless another process still holds the store.
    if std::fs::metadata(beside(path, "-wal")).is_ok_and(|m| m.len() > 0) {
        let _ = std::fs::remove_file(&new);
        return Err("the damaged store still holds a -wal".into());
    }
    if let Err(e) = std::fs::copy(path, aside) {
        let _ = std::fs::remove_file(aside);
        let _ = std::fs::remove_file(&new);
        return Err(format!("copy aside: {e}"));
    }
    if let Err(e) = std::fs::rename(&new, path) {
        // The old store stays: its copy is a backup of nothing.
        let _ = std::fs::remove_file(aside);
        let _ = std::fs::remove_file(&new);
        return Err(format!("rename over: {e}"));
    }
    prune_asides(path);
    Ok(())
}

#[cfg(test)]
#[path = "../../tests/unit/settings/swap.rs"]
mod tests;
