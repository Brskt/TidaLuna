#[cfg(test)]
use super::current_track_fixture;
use std::sync::{Arc, LazyLock, Mutex};

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
/// happens to see: "still equal to the stamp" means nothing minted since, which "equal to
/// whatever was read a moment ago" cannot say.
///
/// It stays out of `TrackInfo`, whose hand-written `PartialEq` already has to exclude
/// `product_id`: a second excluded field, in the struct whose equality decides every preload hit,
/// is how that trap bites twice instead of once.
#[derive(Clone, Debug)]
pub struct RetainedTrack {
    pub track: TrackInfo,
    pub load_gen: u32,
}

/// Which load last published the listener's source. Compared for equality only: generations
/// wrap.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Publication(u32);

/// What a promotion did with the slot.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[must_use]
pub enum PromotionVerdict {
    /// Nothing was published since the fade armed: the promoted track is the source now.
    Published,
    /// A load published after the fade armed. It owns the slot, and whoever asked for it has
    /// already left the track this fade was leaving.
    Superseded,
}

/// The source the listener's pipeline plays or is being loaded onto, stamped with the load that
/// published it. The stamp exists for the absence too: two DASH loads publish two absences, and
/// only the stamp tells the newer one from the one a fade armed over.
#[derive(Debug)]
pub struct CurrentSource {
    retained: Option<RetainedTrack>,
    published: Publication,
}

impl CurrentSource {
    pub(super) const fn new() -> Self {
        Self {
            retained: None,
            published: Publication(0),
        }
    }

    pub fn retained(&self) -> Option<&RetainedTrack> {
        self.retained.as_ref()
    }

    /// In place only: a credential refresh re-signs the source without replacing it.
    pub fn retained_mut(&mut self) -> Option<&mut RetainedTrack> {
        #[cfg(test)]
        current_track_fixture::assert_held("retained_mut");
        self.retained.as_mut()
    }

    pub fn publication(&self) -> Publication {
        self.published
    }

    /// A load's publication, `None` for a source no replay can rebuild.
    pub fn publish_load(&mut self, load_gen: u32, track: Option<TrackInfo>) {
        #[cfg(test)]
        current_track_fixture::assert_held("publish_load");
        self.retained = track.map(|track| RetainedTrack { track, load_gen });
        self.published = Publication(load_gen);
    }

    /// A promotion replaces the source only if nothing was published since its fade armed. It
    /// mints nothing and keeps the stamp: it is not a load.
    pub fn publish_promotion(
        &mut self,
        next: &RetainedTrack,
        armed_over: Publication,
    ) -> PromotionVerdict {
        #[cfg(test)]
        current_track_fixture::assert_held("publish_promotion");
        if self.published != armed_over {
            return PromotionVerdict::Superseded;
        }
        self.retained = Some(next.clone());
        PromotionVerdict::Published
    }
}

pub static CURRENT_TRACK: LazyLock<Arc<Mutex<CurrentSource>>> =
    LazyLock::new(|| Arc::new(Mutex::new(CurrentSource::new())));

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
