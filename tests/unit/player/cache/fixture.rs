//! The cache helpers the sidecars of `src/player/cache/` share, attached once to `cache/mod.rs` by
//! `#[path]`.

use super::evict::Owed;
use super::store::{Reservation, hold_moves_off};
use super::*;
use std::sync::Mutex;

/// The calls the production passes make over a shared cache, for a test's own cache: lent to a
/// `Mutex` for the length of the call, as `store_into` and the applier hold the global one.
impl AudioCache {
    pub(super) fn lent<R>(&mut self, run: impl FnOnce(&Mutex<AudioCache>) -> R) -> R {
        let placeholder = AudioCache::disabled(Path::new(""), 0);
        let shared = Mutex::new(std::mem::replace(self, placeholder));
        let out = run(&shared);
        *self = shared.into_inner().unwrap_or_else(|e| e.into_inner());
        out
    }

    /// `insert_row` and the pass it owes, as `store_into` runs them.
    pub(super) fn record(
        &mut self,
        track_id: &str,
        format: &str,
        file_size: u64,
    ) -> anyhow::Result<StoreOutcome> {
        self.record_with(track_id, format, file_size, &|p| fs::remove_file(p))
    }

    pub(super) fn record_with(
        &mut self,
        track_id: &str,
        format: &str,
        file_size: u64,
        remove: &dyn Fn(&Path) -> std::io::Result<()>,
    ) -> anyhow::Result<StoreOutcome> {
        let (outcome, owed) = self.insert_row(track_id, format, file_size, remove)?;
        self.run_owed(owed, remove)?;
        Ok(outcome)
    }

    /// `commit_row` and the pass it owes, as `store_into` runs them.
    pub(super) fn commit(
        &mut self,
        reservation: Reservation,
        format: &str,
        file_size: u64,
        moved: anyhow::Result<()>,
    ) -> anyhow::Result<StoreOutcome> {
        let (outcome, owed) = self.commit_row(reservation, format, file_size, moved)?;
        self.run_owed(owed, &|p| fs::remove_file(p))?;
        Ok(outcome)
    }

    fn run_owed(
        &mut self,
        owed: Option<Owed>,
        remove: &dyn Fn(&Path) -> std::io::Result<()>,
    ) -> Result<(), CacheError> {
        let Some(owed) = owed else {
            return Ok(());
        };
        self.lent(|cache| AudioCache::evict(cache, Some(&owed.keep), owed.unreclaimable, remove))
    }
}

/// A Clear as the applier runs it: the move gate taken first, then the wipe.
pub(super) fn wipe(cache: &mut AudioCache) -> Result<(), CacheError> {
    let moves_off = hold_moves_off();
    cache.lent(|cache| AudioCache::wipe(cache, &moves_off, &|p| fs::remove_file(p)))
}

/// A staging file in `dir`, holding `bytes`: what a finished download hands over.
pub(super) fn staged(dir: &Path, bytes: &[u8]) -> (tempfile::NamedTempFile, PathBuf) {
    use std::io::Write;
    fs::create_dir_all(dir).unwrap();
    let mut file = tempfile::NamedTempFile::new_in(dir).unwrap();
    file.write_all(bytes).unwrap();
    let path = file.path().to_path_buf();
    (file, path)
}

pub(super) fn generation_of(cache: &AudioCache, id: &str) -> Generation {
    cache.lookup_seen(id).expect("the id has a row").generation
}
