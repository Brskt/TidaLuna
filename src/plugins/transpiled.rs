//! The plugins' transpiled code, kept on disk: a later load of the same source skips the helper,
//! a process per plugin. Out of plugins.db, where a damaged row blocks every delete that cascades
//! onto it; here a damaged file costs one transpile.

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::store::content_hash;
use super::transpile::transform_key;

/// The tag a file opens with, bumped with its layout.
const FORMAT: &str = "luna-js 1";

pub(crate) struct TranspileCache {
    dir: PathBuf,
}

pub(crate) static TRANSPILE_CACHE: std::sync::LazyLock<TranspileCache> =
    std::sync::LazyLock::new(|| {
        TranspileCache::new(
            crate::state::cache_data_dir()
                .join("cache")
                .join("plugin_js"),
        )
    });

/// The name the helper is given for a plugin's source.
pub(crate) fn source_name(url: &str) -> String {
    format!("{url}.mts")
}

/// What a transpile depends on: the transform, the source, and whether its name makes it a
/// declaration file, which oxc empties. Nothing else of the name reaches the output.
struct Key {
    transform: &'static str,
    declaration: bool,
    source_hash: String,
}

impl Key {
    fn new(name: &str, source_hash: String) -> Self {
        let declaration = oxc::span::SourceType::from_path(Path::new(name))
            .is_ok_and(|kind| kind.is_typescript_definition());
        Self {
            transform: transform_key(),
            declaration,
            source_hash,
        }
    }

    fn of(name: &str, code: &str) -> Self {
        Self::new(name, content_hash(code))
    }

    /// One file per key; plugins sharing a source share it, and a store move keeps it.
    fn file_name(&self) -> String {
        let named = format!(
            "{}\0{}\0{}",
            self.transform,
            u8::from(self.declaration),
            self.source_hash
        );
        format!(
            "{:016x}.js",
            twox_hash::XxHash64::oneshot(0, named.as_bytes())
        )
    }

    /// The first line, which names what the rest must be.
    fn header(&self, js: &str) -> String {
        format!(
            "{FORMAT} {} {} {} {}\n",
            self.transform,
            u8::from(self.declaration),
            self.source_hash,
            content_hash(js)
        )
    }
}

/// One file of the cache directory, as the sweep sees it.
struct CacheEntry {
    path: PathBuf,
    modified: SystemTime,
}

/// The files no stored source names. One younger than `STALE_AFTER` stays (an update's, not yet in
/// plugins.db), and so does one dated ahead of `now`.
fn plan_sweep(entries: Vec<CacheEntry>, live: &HashSet<String>, now: SystemTime) -> Vec<PathBuf> {
    entries
        .into_iter()
        .filter(|entry| {
            let named = entry
                .path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| live.contains(name));
            !named
                && now
                    .duration_since(entry.modified)
                    .is_ok_and(|age| age >= crate::video::cover_cache::STALE_AFTER)
        })
        .map(|entry| entry.path)
        .collect()
}

impl TranspileCache {
    fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// The kept transpile of `code` under `name`, or `None`: none kept, or a file not read back
    /// whole, which the hash in its first line catches.
    pub(crate) fn get(&self, name: &str, code: &str) -> Option<String> {
        let key = Key::of(name, code);
        let bytes = match std::fs::read(self.dir.join(key.file_name())) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(e) => {
                crate::vprintln!("[PLUGIN] Kept transpile unreadable, transpiling again: {e}");
                return None;
            }
        };
        let text = String::from_utf8(bytes).ok()?;
        let (_, js) = text.split_once('\n')?;
        text.starts_with(&key.header(js)).then(|| js.to_string())
    }

    /// Keep `js`, transpiled from `code` under `name`. Written whole under a temporary name, then
    /// renamed into place; no sync, as a file a power cut tore fails its first line.
    pub(crate) fn put(&self, name: &str, code: &str, js: &str) -> std::io::Result<()> {
        let key = Key::of(name, code);
        let path = self.dir.join(key.file_name());
        // A directory planted at the name would hold the rename's retries for their whole budget.
        if std::fs::symlink_metadata(&path).is_ok_and(|meta| !meta.is_file()) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "the name is taken by something that is not a file",
            ));
        }
        std::fs::create_dir_all(&self.dir)?;
        let mut staged = tempfile::NamedTempFile::new_in(&self.dir)?;
        staged.write_all(key.header(js).as_bytes())?;
        staged.write_all(js.as_bytes())?;
        crate::video::codec::persist_outlasting_holds(staged, &path)
    }

    /// Delete the files of sources no longer stored, an update's old version or an uninstalled
    /// plugin's. Files only, never a directory or a link.
    fn sweep(&self, sources: &[(String, String)]) {
        let live: HashSet<String> = sources
            .iter()
            .map(|(url, hash)| Key::new(&source_name(url), hash.clone()).file_name())
            .collect();
        let Ok(listing) = std::fs::read_dir(&self.dir) else {
            return;
        };
        let entries = listing
            .flatten()
            .filter_map(|entry| {
                let meta = entry.metadata().ok().filter(|meta| meta.is_file())?;
                Some(CacheEntry {
                    path: entry.path(),
                    modified: meta.modified().ok()?,
                })
            })
            .collect();
        for path in plan_sweep(entries, &live, SystemTime::now()) {
            if let Err(e) = std::fs::remove_file(&path) {
                crate::vprintln!("[PLUGIN] Deleting a kept transpile failed: {e}");
            }
        }
    }
}

/// Once per process, at its first load pass; what updates leave behind later waits for the next
/// launch. A store that cannot be read sweeps nothing.
pub(crate) fn sweep_once() {
    static SWEPT: std::sync::Once = std::sync::Once::new();
    SWEPT.call_once(|| {
        match crate::state::db().call_plugins(|pc| super::store::stored_sources(pc)) {
            Ok(sources) => TRANSPILE_CACHE.sweep(&sources),
            Err(e) => crate::vprintln!("[PLUGIN] Kept transpiles not swept: {e}"),
        }
    });
}

#[cfg(test)]
#[path = "../../tests/unit/plugins/transpiled.rs"]
mod tests;
