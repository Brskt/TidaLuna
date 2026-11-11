//! Obtaining Cisco's prebuilt OpenH264 library.
//!
//! The library is downloaded after install rather than bundled, because Cisco's AVC patent
//! coverage attaches to the binary Cisco itself builds and not to source anyone else compiles.
//! The four conditions that coverage carries are load-bearing here: separate download, a user
//! switch, the attribution string beside that switch, and Cisco's full notice with condition 4
//! wherever licensing information is shown.

mod install;
mod supervisor;

pub(crate) use install::{load_api, persist_outlasting_holds, sha256_bytes, write_chain};
pub(crate) use supervisor::{
    decoder, is_ready, note_demand, recheck, set_enabled, start, state_json,
};

/// Why the codec failed, apart from whether to try again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FailureClass {
    /// The host could not be reached, or answered that it could not serve right now.
    Network,
    /// The host answered, and the answer was no.
    Refused,
    /// The bytes arrived and were not the codec.
    Corrupt,
    /// Every local fault (a full disk, a permission, a file another program holds).
    Disk,
    /// The verified file would not load in this process.
    Unloadable,
    /// A supervisor step panicked: a bug of ours, whose cause says nothing of whether it recurs.
    Crashed,
}

impl FailureClass {
    /// A host that answered without success: it could not serve right now, or it said no.
    pub(crate) fn of_status(status: reqwest::StatusCode) -> Self {
        if status.is_server_error() || matches!(status.as_u16(), 408 | 429) {
            Self::Network
        } else {
            Self::Refused
        }
    }

    /// The only failure text a class sends out of the process, a constant: no path or OS
    /// message travels with it to the page.
    fn phrase(self) -> &'static str {
        match self {
            Self::Network => "Cisco's server could not be reached",
            Self::Refused => "Cisco's server did not provide the video codec",
            Self::Corrupt => "the downloaded video codec failed its integrity check",
            Self::Disk => {
                "the video codec file could not be read or written: another program may hold it, or the disk refuses it"
            }
            Self::Unloadable => "the video codec could not be loaded on this system",
            Self::Crashed => "the video codec service hit an internal error",
        }
    }
}

/// `f` on the blocking pool, where a hash or a `dlopen` does not hold a runtime worker every other
/// task shares. `None` when the runtime cancelled it: nothing aborts these handles, which leaves
/// a shutdown as the only way there. A panic still unwinds the caller, up to `Core::step`.
async fn off_runtime<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    match tokio::task::spawn_blocking(f).await {
        Ok(value) => Some(value),
        Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
        Err(_) => None,
    }
}
