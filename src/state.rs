use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

/// Chromium version CEF wraps, `MAJOR.MINOR.BUILD.PATCH`. This is not the `cef` crate
/// version: the crate tracks CEF releases, which wrap their own Chromium. One owner keeps
/// the user agent and the startup log from disagreeing about which browser this is.
pub(crate) fn chromium_version() -> String {
    format!(
        "{}.{}.{}.{}",
        cef::sys::CHROME_VERSION_MAJOR,
        cef::sys::CHROME_VERSION_MINOR,
        cef::sys::CHROME_VERSION_BUILD,
        cef::sys::CHROME_VERSION_PATCH,
    )
}

fn build_user_agent(os_token: &str) -> String {
    format!(
        "Mozilla/5.0 ({os}) AppleWebKit/537.36 (KHTML, like Gecko) TidaLunar/{ver} Chrome/{chrome} Safari/537.36",
        os = os_token,
        ver = env!("CARGO_PKG_VERSION"),
        chrome = chromium_version(),
    )
}

pub(crate) static USER_AGENT: LazyLock<String> = LazyLock::new(|| {
    let os = if cfg!(target_os = "linux") {
        "X11; Linux x86_64"
    } else {
        "Windows NT 10.0; WOW64"
    };
    build_user_agent(os)
});

// TIDAL's bar component renders only when navigator.userAgent contains a
// Windows OS token. We override at the JS layer: HTTP traffic keeps the
// honest Linux UA above.
#[cfg(target_os = "linux")]
pub(crate) static JS_USER_AGENT: LazyLock<String> =
    LazyLock::new(|| build_user_agent("Windows NT 10.0; WOW64"));

/// A retained source: what to fetch it with, and the id the frontend knows it by. The id lives
/// here because the paths that replay a retained source (a re-arm, a recover) are handed no id
/// of their own, and a measurement taken on a replay must still name its track.
#[derive(Clone, Debug, Eq)]
pub struct TrackInfo {
    pub url: String,
    pub key: String,
    pub format: String,
    pub product_id: Option<String>,
}

/// Equality answers "is this the same source", which is what preload matching asks, and the id
/// has no part in that. It must not: the preload delegate strips identity before Rust sees it;
/// a preloaded copy carries `None` while the load that comes to claim it carries the real id.
/// Comparing ids here would make every gapless preload hit miss.
impl PartialEq for TrackInfo {
    fn eq(&self, other: &Self) -> bool {
        self.url == other.url && self.key == other.key && self.format == other.format
    }
}

#[derive(Clone, Debug, Default)]
pub struct TrackMetadata {
    pub title: String,
    pub artist: String,
    pub quality: String,
    /// Tells one track from the next. `None` when the payload carried no id, which no
    /// comparison may read as a match: two unidentified tracks are not the same track.
    pub id: Option<String>,
}

pub static CURRENT_METADATA: LazyLock<Arc<Mutex<Option<TrackMetadata>>>> =
    LazyLock::new(|| Arc::new(Mutex::new(None)));

/// A retained source together with the load generation it was published under.
///
/// The pairing is the whole point. The generation and this slot are two separate writes, and a
/// reader taking them one at a time can hold a stale track beside a fresh generation that every
/// freshness guard in the player passes, since they all test the generation alone. The replay it
/// authorises then overwrites a newer load: `Player::rearm` skips the identity guard
/// `Player::load` applies, and `load_with_policy` is unconditional, aborting that load's task and
/// cancelling its download. One slot makes the pair coherent by construction.
///
/// `load_gen` is what this slot's own publication was stamped with, never what a later reader
/// happens to see. The generation being monotone, "still equal to the stamp" is strictly stronger
/// than "equal to whatever was read a moment ago".
///
/// It stays out of `TrackInfo`, whose hand-written `PartialEq` already has to exclude
/// `product_id`: a second excluded field, in the struct whose equality decides every preload hit,
/// is how that trap bites twice instead of once.
#[derive(Clone, Debug)]
pub struct RetainedTrack {
    pub track: TrackInfo,
    pub load_gen: u32,
}

pub static CURRENT_TRACK: LazyLock<Arc<Mutex<Option<RetainedTrack>>>> =
    LazyLock::new(|| Arc::new(Mutex::new(None)));

/// Shared client tuning (UA, timeouts, pooling, HTTP/2, TLS) minus the cookie
/// configuration, which the global and per-plugin clients set differently.
fn base_client_builder() -> reqwest::ClientBuilder {
    // Keep streaming requests unconstrained (no global request timeout),
    // but tune connection setup and pooling for lower latency variance.
    reqwest::Client::builder()
        .user_agent(USER_AGENT.as_str())
        .connect_timeout(Duration::from_secs(8))
        .pool_idle_timeout(Duration::from_secs(90))
        .pool_max_idle_per_host(8)
        .tcp_nodelay(true)
        // HTTP/2: adaptive flow-control window for better throughput
        .http2_adaptive_window(true)
        // HTTP/2: keep-alive pings to detect dead connections
        .http2_keep_alive_interval(Duration::from_secs(10))
        .http2_keep_alive_timeout(Duration::from_secs(5))
        .http2_keep_alive_while_idle(true)
        // TLS: accept 1.2 and 1.3
        .min_tls_version(reqwest::tls::Version::TLS_1_2)
}

fn build_http_client() -> reqwest::Client {
    base_client_builder()
        .cookie_store(true)
        .build()
        .expect("failed to build HTTP client")
}

/// The clients that stream an encrypted media body, which is the only traffic here with an
/// unbounded await on a body that a silent server can hold open forever.
///
/// A whole-request timeout is the wrong shape, one body being a whole track. `read_timeout`'s
/// clock is created inside `poll_frame` and torn down on every delivered frame, so the stretch
/// the governor spends not polling costs no budget, and its separate one-shot deadline ends a
/// server that accepts the connection and then answers nothing. 15s clears the 8s connect budget
/// with room for a cold CDN edge's TTFB. Deliberately not on `base_client_builder`: the plugin
/// egress clients layer on that one, and a plugin's destination may answer at any cadence.
fn build_media_client() -> reqwest::Client {
    base_client_builder()
        .cookie_store(true)
        .read_timeout(Duration::from_secs(15))
        .build()
        .expect("failed to build media HTTP client")
}

/// Build a native-plugin egress client with a caller-owned cookie jar (for
/// per-plugin isolation) and a redirect policy (follow vs manual/error).
pub(crate) fn build_native_client(
    jar: Arc<reqwest::cookie::Jar>,
    redirect: reqwest::redirect::Policy,
) -> reqwest::Client {
    base_client_builder()
        .cookie_provider(jar)
        .redirect(redirect)
        .build()
        .expect("failed to build native HTTP client")
}

/// Build a client with a redirect policy of the caller's choosing, sharing the base tuning. Used by
/// `plugin.download`, whose policy re-checks the host at every hop.
pub(crate) fn build_client_with_redirect(redirect: reqwest::redirect::Policy) -> reqwest::Client {
    base_client_builder()
        .redirect(redirect)
        .build()
        .expect("failed to build redirect-checked HTTP client")
}

pub static HTTP_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(build_http_client);

/// Dedicated HTTP client for playback (initial GET + Range restarts).
/// Separate from HTTP_CLIENT; playback gets its own TCP connection,
/// avoiding HTTP/2 bandwidth contention with preload downloads.
pub static HTTP_CLIENT_PLAYBACK: LazyLock<reqwest::Client> = LazyLock::new(build_media_client);

/// Preload's own client, kept off `HTTP_CLIENT` for the read timeout rather than for contention:
/// its download loop has the same governor-then-chunk shape as playback's and the same await a
/// silent server can hold open, while `HTTP_CLIENT` also serves `plugin.fetch`.
pub static HTTP_CLIENT_PRELOAD: LazyLock<reqwest::Client> = LazyLock::new(build_media_client);

pub struct PreloadedTrack {
    pub track: TrackInfo,
    /// The decoded bytes AND the CDN's ciphertext, both living in the buffer's own shared
    /// state. Held as a `RamBuffer` rather than a loose `Vec<u8>` plus a non-`Clone` tempfile,
    /// so a caller can take a cheap second reader to inspect the track without consuming it:
    /// handed out separately, an inspection either copied the whole payload or dropped the
    /// ciphertext, and a dropped one silently costs the track its disk-cache entry.
    pub buffer: crate::player::buffer::RamBuffer,
}

// `PreloadState` and its `PRELOAD_STATE` static live in `crate::audio::preload`, beside the
// only code that may write them: every field is private, and Rust grants that access to the
// defining module alone. Keeping the type here would have meant either public fields (the
// shape that produced the defects) or splitting each transition from the reasoning that
// justifies it.

pub static GOVERNOR: LazyLock<crate::audio::bandwidth::GovernorHandle> =
    LazyLock::new(crate::audio::bandwidth::spawn_governor);

pub fn cache_data_dir() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        let base = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(base).join("tidalunar")
    }
    #[cfg(not(target_os = "windows"))]
    {
        let base = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(base)
            .join(".local")
            .join("share")
            .join("tidalunar")
    }
}

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
    let dir = cache_data_dir();
    let cache = match crate::player::cache::AudioCache::open(&dir) {
        Ok(cache) => {
            crate::vprintln!("[CACHE]  Opened ({})", dir.join("cache").display());
            cache
        }
        Err(e) => {
            crate::verr!("[CACHE]  Failed to open: {e}");
            // Fallback: a session cache in the OS temp dir.
            let tmp = std::env::temp_dir().join("tidalunar");
            crate::player::cache::AudioCache::open(&tmp).unwrap_or_else(|e| {
                // Never panic here: a panicking init poisons the LazyLock for
                // the process lifetime; every later touch would panic too.
                crate::verr!("[CACHE]  Fallback failed, cache disabled: {e}");
                crate::player::cache::AudioCache::disabled(&dir)
            })
        }
    };
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
pub(crate) mod current_track_fixture {
    /// Poison is recovered, never propagated: one failing assertion must not turn every later
    /// test red for a reason that is not its own.
    static SLOT: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Lock ORDER where a test holds both: `PRELOAD_TESTS` first, this one second. Taking
    /// them the other way round in a future test deadlocks against
    /// `a_cancelled_fade_leaves_the_next_track_staged_and_a_promotion_spends_it`, the one
    /// test that holds both today.
    pub(crate) struct CurrentTrackSlot {
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl CurrentTrackSlot {
        /// The slot empty, for a test that needs no track published.
        pub(crate) fn clear() -> Self {
            let slot = Self {
                _lock: SLOT.lock().unwrap_or_else(|e| e.into_inner()),
            };
            *super::CURRENT_TRACK
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = None;
            slot
        }

        /// The slot holding a published track, for a test that needs one to exist rather than
        /// to be absent. Same exclusion, opposite starting state.
        pub(crate) fn holding(retained: super::RetainedTrack) -> Self {
            let slot = Self {
                _lock: SLOT.lock().unwrap_or_else(|e| e.into_inner()),
            };
            *super::CURRENT_TRACK
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(retained);
            slot
        }
    }

    /// Restores the slot even when the test panics, keeping a failure from leaking into
    /// whichever test the harness runs next.
    impl Drop for CurrentTrackSlot {
        fn drop(&mut self) {
            *super::CURRENT_TRACK
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = None;
        }
    }
}
