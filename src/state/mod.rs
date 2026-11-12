mod net;
mod paths;
mod track;

use paths::APP_DIR;
use std::sync::{LazyLock, Mutex};
#[cfg(test)]
use track::CurrentSource;

pub use net::{HTTP_CLIENT, HTTP_CLIENT_PLAYBACK, HTTP_CLIENT_PRELOAD};
pub(crate) use net::{
    HTTP_CLIENT_SOURCE, USER_AGENT, build_host_checked_client, build_native_client,
    chromium_version,
};
pub use paths::cache_data_dir;
#[cfg(target_os = "linux")]
pub(crate) use paths::user_home;
#[cfg(not(target_os = "macos"))]
pub(crate) use paths::{InstallDir, install_dir};
pub(crate) use paths::{data_dir, home_folder};
pub use track::{
    CURRENT_METADATA, CURRENT_TRACK, PreloadedTrack, PromotionVerdict, Publication, RetainedTrack,
    TrackInfo, TrackMetadata,
};

/// One directory per test process, `<temp>/tidalunar-test-<pid>-<nonce>`, swept by the next
/// process once its owner is gone. Not removed at exit: the statics holding files open in it (the
/// audio cache's SQLite index) are never dropped, and Windows refuses to delete an open file.
#[cfg(test)]
pub(crate) mod test_root;

pub(crate) static RT_HANDLE: std::sync::OnceLock<tokio::runtime::Handle> =
    std::sync::OnceLock::new();

pub(crate) fn rt_handle() -> &'static tokio::runtime::Handle {
    RT_HANDLE
        .get()
        .expect("Tokio runtime handle not initialized")
}

pub(crate) static DB: std::sync::OnceLock<crate::db::DbActor> = std::sync::OnceLock::new();

pub(crate) static NATIVE_RUNTIME: std::sync::OnceLock<crate::native_runtime::NativeRuntime> =
    std::sync::OnceLock::new();
pub(crate) static NATIVE_RUNTIME_INIT: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub(crate) fn db() -> &'static crate::db::DbActor {
    DB.get().expect("DB actor not initialized")
}

pub(crate) static BOOT_SETTINGS: std::sync::OnceLock<crate::settings::BootSettings> =
    std::sync::OnceLock::new();

pub(crate) fn boot_settings() -> &'static crate::settings::BootSettings {
    BOOT_SETTINGS.get().expect("BootSettings not initialized")
}

pub static AUDIO_CACHE: LazyLock<Mutex<crate::player::cache::AudioCache>> = LazyLock::new(|| {
    use crate::player::cache::{AudioCache, DEFAULT_CAPACITY_GB, capacity_bytes};
    let dir = cache_data_dir();
    // `get`, not `boot_settings()`: tests reach this global without boot settings, and a
    // panicking init would poison it. They get the defaults.
    let (enabled, gb) = BOOT_SETTINGS
        .get()
        .map_or((true, DEFAULT_CAPACITY_GB), |boot| {
            (boot.audio_cache_enabled, boot.audio_cache_gb)
        });
    let max_bytes = capacity_bytes(gb);
    let mut cache = match AudioCache::open_with_capacity(&dir, max_bytes) {
        Ok(cache) => {
            crate::vprintln!("[CACHE]  Opened ({})", dir.join("cache").display());
            cache
        }
        Err(e) => {
            crate::verr!("[CACHE]  Failed to open: {e}");
            // Fallback: a session cache in the OS temp dir.
            let tmp = std::env::temp_dir().join(APP_DIR);
            AudioCache::open_with_capacity(&tmp, max_bytes).unwrap_or_else(|e| {
                // Never panic here: a panicking init poisons the LazyLock for
                // the process lifetime; every later touch would panic too.
                crate::verr!("[CACHE]  Fallback failed, cache disabled: {e}");
                AudioCache::disabled(&dir, max_bytes)
            })
        }
    };
    cache.set_enabled(enabled);
    // After the boot writes above: the seed has no page to reach, which pulls at mount.
    crate::player::cache::install_publisher(&mut cache);
    Mutex::new(cache)
});

/// The exclusion every test that touches [`CURRENT_TRACK`] has to hold, and the fixture that
/// holds it for them.
///
/// The slot is one global for the whole test binary and libtest runs several threads wide, so a
/// test that publishes a track and then asserts on it races every other test that clears the same
/// slot. Not a theory: it made the ASIO re-arm test fail on Windows while `--test-threads=1`
/// passed 728 of 728. The lock is held by the FIXTURE rather than taken by each test, so a test
/// cannot forget a lock it never has to mention: the only way to touch the slot is to construct
/// one of these.
#[cfg(test)]
pub(crate) mod current_track_fixture;

/// Opens the process globals a download or a load takes first, ahead of any clock: their one-time
/// IO (the cache's SQLite, each client's root store) runs on the thread that takes them, and a
/// test timing a window around it would time a slow disk as the code under test.
#[cfg(test)]
pub(crate) fn warm_process_globals() {
    LazyLock::force(&AUDIO_CACHE);
    LazyLock::force(&HTTP_CLIENT_PRELOAD);
    LazyLock::force(&HTTP_CLIENT_PLAYBACK);
}

/// Bounds a test's wait that only a hang exceeds: no claim about speed rides on it.
#[cfg(test)]
pub(crate) const HANG_GUARD: std::time::Duration = std::time::Duration::from_secs(30);
