//! A controller's change to the queue's shuffle or repeat mode, and the queue
//! server call that makes it (TIDAL's `QueueWindowLoader::loadWithShuffle` and
//! `loadWithRepeatMode`).

use super::QueueError;
use super::http::{ConnectRoot, RootedUrl};
use crate::connect::types::{QueueChangeReason, QueueInfo, RepeatMode};

/// A click on this host's shuffle or repeat button while a controller casts. It names no mode:
/// the receiver takes the next one from the cast's own, which the phone shows too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModeToggle {
    Shuffle,
    Repeat,
}

impl ModeToggle {
    pub(super) fn change_from(self, info: &QueueInfo) -> QueueChange {
        match self {
            Self::Shuffle => QueueChange::Shuffle(!info.shuffled),
            Self::Repeat => QueueChange::Repeat(info.repeat_mode.next()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum QueueChange {
    Shuffle(bool),
    Repeat(RepeatMode),
}

impl QueueChange {
    /// A change the queue already shows: TIDAL's receiver answers it done, with no call.
    pub(super) fn is_in(self, info: &QueueInfo) -> bool {
        match self {
            Self::Shuffle(shuffle) => info.shuffled == shuffle,
            Self::Repeat(mode) => info.repeat_mode == mode,
        }
    }

    pub(super) fn applied_to(self, info: &QueueInfo) -> QueueInfo {
        let mut changed = info.clone();
        match self {
            Self::Shuffle(shuffle) => changed.shuffled = shuffle,
            Self::Repeat(mode) => changed.repeat_mode = mode,
        }
        changed
    }

    pub(super) fn reason(self) -> QueueChangeReason {
        match self {
            Self::Shuffle(_) => QueueChangeReason::ShuffleChanged,
            Self::Repeat(_) => QueueChangeReason::RepeatModeChanged,
        }
    }

    /// The queue server resource: `{server_url}/{queueId}/shuffle` or `.../repeatmode`.
    pub(super) fn url(self, root: &ConnectRoot, queue_id: &str) -> Result<RootedUrl, QueueError> {
        let resource = match self {
            Self::Shuffle(_) => "shuffle",
            Self::Repeat(_) => "repeatmode",
        };
        root.url(&[queue_id, resource], &[])
    }

    /// The PUT body. A shuffle names the item playing, which the server keeps current.
    pub(super) fn body(self, current_item: &str) -> serde_json::Value {
        match self {
            Self::Shuffle(shuffle) => {
                serde_json::json!({ "shuffle": shuffle, "current_item": current_item })
            }
            Self::Repeat(mode) => serde_json::json!({ "repeat_mode": mode.queue_server_word() }),
        }
    }
}
