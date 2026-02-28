use once_cell::sync::Lazy;
use std::sync::{Arc, Mutex};
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

pub static HTTP_CLIENT: Lazy<reqwest::Client> = Lazy::new(reqwest::Client::new);

#[derive(Debug)]
pub struct PreloadedTrack {
    pub track: TrackInfo,
    pub data: Vec<u8>,
}

#[derive(Debug)]
pub struct PreloadState {
    pub task: Option<tokio::task::JoinHandle<()>>,
    pub data: Option<PreloadedTrack>,
}

pub static PRELOAD_STATE: Lazy<TokioMutex<PreloadState>> = Lazy::new(|| {
    TokioMutex::new(PreloadState {
        task: None,
        data: None,
    })
});

pub static GOVERNOR: Lazy<crate::bandwidth::GovernorHandle> =
    Lazy::new(crate::bandwidth::spawn_governor);
