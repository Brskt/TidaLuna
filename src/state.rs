use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;
use tokio::sync::Mutex as TokioMutex;

fn build_user_agent(os_token: &str) -> String {
    format!(
        "Mozilla/5.0 ({os}) AppleWebKit/537.36 (KHTML, like Gecko) TidaLunar/{ver} Chrome/{cmaj}.{cmin}.{cbld}.{cpat} Safari/537.36",
        os = os_token,
        ver = env!("CARGO_PKG_VERSION"),
        cmaj = cef::sys::CHROME_VERSION_MAJOR,
        cmin = cef::sys::CHROME_VERSION_MINOR,
        cbld = cef::sys::CHROME_VERSION_BUILD,
        cpat = cef::sys::CHROME_VERSION_PATCH,
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
// Windows OS token. We override at the JS layer so HTTP traffic keeps the
// honest Linux UA above.
#[cfg(target_os = "linux")]
pub(crate) static JS_USER_AGENT: LazyLock<String> =
    LazyLock::new(|| build_user_agent("Windows NT 10.0; WOW64"));

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrackInfo {
    pub url: String,
    pub key: String,
    pub format: String,
}

#[derive(Clone, Debug, Default)]
pub struct TrackMetadata {
    pub title: String,
    pub artist: String,
    pub quality: String,
}

pub static CURRENT_METADATA: LazyLock<Arc<Mutex<Option<TrackMetadata>>>> =
    LazyLock::new(|| Arc::new(Mutex::new(None)));

pub static CURRENT_TRACK: LazyLock<Arc<Mutex<Option<TrackInfo>>>> =
    LazyLock::new(|| Arc::new(Mutex::new(None)));

fn build_http_client() -> reqwest::Client {
    // Keep streaming requests unconstrained (no global request timeout),
    // but tune connection setup and pooling for lower latency variance.
    reqwest::Client::builder()
        .cookie_store(true)
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
        .build()
        .expect("failed to build HTTP client")
}

pub static HTTP_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(build_http_client);

/// Dedicated HTTP client for playback (initial GET + Range restarts).
/// Separate from HTTP_CLIENT so playback gets its own TCP connection,
/// avoiding HTTP/2 bandwidth contention with preload downloads.
pub static HTTP_CLIENT_PLAYBACK: LazyLock<reqwest::Client> = LazyLock::new(build_http_client);

#[derive(Debug)]
pub struct PreloadedTrack {
    pub track: TrackInfo,
    pub data: Vec<u8>,
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

pub static AUDIO_CACHE: LazyLock<Mutex<crate::player::cache::AudioCache>> = LazyLock::new(|| {
    let dir = cache_data_dir();
    match crate::player::cache::AudioCache::open(&dir) {
        Ok(cache) => {
            crate::vprintln!("[CACHE]  Opened ({})", dir.join("cache").display());
            Mutex::new(cache)
        }
        Err(e) => {
            eprintln!("[CACHE]  Failed to open: {e}");
            // Fallback: in-memory-only cache (no persistence)
            Mutex::new(
                crate::player::cache::AudioCache::open(&std::env::temp_dir().join("tidalunar"))
                    .expect("fallback cache init failed"),
            )
        }
    }
});
