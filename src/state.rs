use once_cell::sync::Lazy;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Mutex as TokioMutex;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrackInfo {
    pub url: String,
    pub key: String,
}

#[derive(Clone, Debug, Default)]
pub struct TrackMetadata {
    pub title: String,
    pub artist: String,
    pub quality: String,
}

pub static CURRENT_METADATA: Lazy<Arc<Mutex<Option<TrackMetadata>>>> =
    Lazy::new(|| Arc::new(Mutex::new(None)));

pub static CURRENT_TRACK: Lazy<Arc<Mutex<Option<TrackInfo>>>> =
    Lazy::new(|| Arc::new(Mutex::new(None)));

fn build_http_client() -> reqwest::Client {
    // Keep streaming requests unconstrained (no global request timeout),
    // but tune connection setup and pooling for lower latency variance.
    reqwest::Client::builder()
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

pub static HTTP_CLIENT: Lazy<reqwest::Client> = Lazy::new(build_http_client);

/// Dedicated HTTP client for playback (initial GET + Range restarts).
/// Separate from HTTP_CLIENT so playback gets its own TCP connection,
/// avoiding HTTP/2 bandwidth contention with preload downloads.
pub static HTTP_CLIENT_PLAYBACK: Lazy<reqwest::Client> = Lazy::new(build_http_client);

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

pub static PRELOAD_STATE: Lazy<TokioMutex<PreloadState>> = Lazy::new(|| {
    TokioMutex::new(PreloadState {
        task: None,
        data: None,
        next_track: None,
    })
});

pub static GOVERNOR: Lazy<crate::bandwidth::GovernorHandle> =
    Lazy::new(crate::bandwidth::spawn_governor);

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

pub static AUDIO_CACHE: Lazy<Mutex<crate::player::cache::AudioCache>> = Lazy::new(|| {
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
