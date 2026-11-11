use crate::audio::bandwidth::GovernorHandle;
use crate::player::buffer::RamBuffer;
use crate::player::{MediaErrorCode, PlayerCommand, cache, current_gen};
use std::sync::mpsc;
use tokio_util::sync::CancellationToken;

mod cache_entry;
mod request;
mod sources;

pub(super) use cache_entry::{CacheReadError, read_cache_entry};
pub(crate) use sources::{cdn_cache_status, log_response_headers};

/// The media segments of a DASH load, never none: an empty list loads as a header-only fMP4 and
/// "completes" in zero samples. The type is the refusal; a caller meets it where it builds
/// the list, and the load itself has nothing left to refuse.
#[derive(Debug, PartialEq)]
pub(crate) struct SegmentUrls(Vec<String>);

impl SegmentUrls {
    /// `None` for an empty list.
    pub(crate) fn new(urls: Vec<String>) -> Option<Self> {
        (!urls.is_empty()).then_some(Self(urls))
    }

    fn len(&self) -> usize {
        self.0.len()
    }
}

impl IntoIterator for SegmentUrls {
    type Item = String;
    type IntoIter = std::vec::IntoIter<String>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum ResumePolicy {
    Disabled,
    /// Start at 0 and discard a seek queued against this track while its device was
    /// released. Distinct from `Disabled`, which a re-arm also sends when it merely
    /// has no position to offer: only this variant expresses the SDK's load contract.
    Restart,
    Auto,
    Explicit(f64),
}

pub(super) struct LoadRequest {
    pub(super) buffer: RamBuffer,
    pub(super) load_gen: u32,
    pub(super) seq: u32,
    pub(super) track_id: String,
    /// The frontend's own id for this track, distinct from `track_id`: that one is the
    /// canonical URL, which the frontend never sees and cannot match a measurement against.
    pub(super) product_id: Option<String>,
    pub(super) resume_policy: ResumePolicy,
    pub(super) load_start: std::time::Instant,
    pub(super) cached: Option<cache::Generation>,
    pub(super) format: String,
}

enum LoadStep {
    Handled,
    Miss,
}

struct LoadContext {
    load_gen: u32,
    event_seq: u32,
    load_start: std::time::Instant,
    resume_policy: ResumePolicy,
    auto_play: bool,
    cmd_tx: mpsc::Sender<PlayerCommand>,
    format: String,
    product_id: Option<String>,
    /// Cancelled when a newer load supersedes this one or on stop.
    cancel_token: CancellationToken,
    /// The player's governor, for the download this load opens.
    governor: GovernorHandle,
}

impl LoadContext {
    fn is_stale(&self) -> bool {
        current_gen() != self.load_gen
    }

    fn publish_load(&self, buffer: RamBuffer, cached: Option<cache::Generation>, track_id: String) {
        let _ = self.cmd_tx.send(PlayerCommand::Load {
            request: LoadRequest {
                buffer,
                load_gen: self.load_gen,
                seq: self.event_seq,
                track_id,
                product_id: self.product_id.clone(),
                resume_policy: self.resume_policy,
                load_start: self.load_start,
                cached,
                format: self.format.clone(),
            },
            auto_play: self.auto_play,
        });
    }

    fn fail_load(&self, error: String, code: MediaErrorCode) {
        let _ = self.cmd_tx.send(PlayerCommand::LoadFailed {
            error,
            code,
            seq: self.event_seq,
            load_gen: self.load_gen,
            product_id: self.product_id.clone(),
        });
    }
}

/// Sends `LoadSettled{generation}` on Drop: the thread's in-flight marker is
/// cleared on every task exit (completion, return, panic, cancellation) - a
/// panicked/aborted load can't leave a play deferring forever. Gen-matched on
/// the thread side; a stale settle is a no-op.
struct LoadSettleGuard {
    cmd_tx: mpsc::Sender<PlayerCommand>,
    generation: u32,
}

impl Drop for LoadSettleGuard {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(PlayerCommand::LoadSettled {
            generation: self.generation,
        });
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/player/load.rs"]
mod tests;
