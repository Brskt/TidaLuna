use once_cell::sync::Lazy;
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
        .build()
        .expect("failed to build HTTP client")
}

pub static HTTP_CLIENT: Lazy<reqwest::Client> = Lazy::new(build_http_client);

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
