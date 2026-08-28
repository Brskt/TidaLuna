use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;
use tokio::sync::Mutex as TokioMutex;

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
/// has no part in that. It must not: the preload delegate strips identity before Rust sees it,
/// so a preloaded copy carries `None` while the load that comes to claim it carries the real id.
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

pub static CURRENT_TRACK: LazyLock<Arc<Mutex<Option<TrackInfo>>>> =
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
/// A whole-request timeout is the wrong shape: one body is a whole track. `read_timeout` is the
/// right one. Its clock is created inside `poll_frame` and torn down on every delivered frame,
/// so the stretch the governor spends not polling between chunks costs no budget at all. It
/// also bounds the wait for response headers, through a separate one-shot deadline, which is
/// what ends a server that accepts the connection and then answers nothing.
///
/// 15s clears the 8s connect budget above with room for a cold CDN edge's TTFB. Deliberately not
/// on `base_client_builder`: the plugin egress clients layer on that one, and a plugin's chosen
/// destination may answer at whatever cadence it likes.
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

#[derive(Debug)]
pub struct PreloadedTrack {
    pub track: TrackInfo,
    pub data: Vec<u8>,
    /// The CDN's ciphertext for `data`, carried for a preload hit to still
    /// populate the disk cache (which stores ciphertext, not playable audio).
    pub ciphertext: Option<(tempfile::NamedTempFile, u64)>,
}

#[derive(Debug)]
pub struct PreloadState {
    pub task: Option<tokio::task::JoinHandle<()>>,
    pub data: Option<PreloadedTrack>,
    pub next_track: Option<TrackInfo>,
}

pub static PRELOAD_STATE: LazyLock<TokioMutex<PreloadState>> = LazyLock::new(|| {
    TokioMutex::new(PreloadState {
        task: None,
        data: None,
        next_track: None,
    })
});

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
