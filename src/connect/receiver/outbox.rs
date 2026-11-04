use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use super::playback::PlaybackNotifyEvent;
use super::queue::QueueNotifyEvent;
use super::session::SessionNotifyEvent;

/// What a service asks of the routing loop: a word to phones, a socket to close, or work for
/// another service. The queue's is boxed: the media it hands over dwarfs the other variants.
pub(crate) enum Effect {
    Session(SessionNotifyEvent),
    Playback(PlaybackNotifyEvent),
    Queue(Box<QueueNotifyEvent>),
}

impl From<SessionNotifyEvent> for Effect {
    fn from(event: SessionNotifyEvent) -> Self {
        Self::Session(event)
    }
}

impl From<PlaybackNotifyEvent> for Effect {
    fn from(event: PlaybackNotifyEvent) -> Self {
        Self::Playback(event)
    }
}

impl From<QueueNotifyEvent> for Effect {
    fn from(event: QueueNotifyEvent) -> Self {
        Self::Queue(Box::new(event))
    }
}

/// The services' effects, in the order they asked for them, applied by the routing loop before
/// the turn that made them ends. Never bounded nor waited on: the loop empties it, and a service
/// waiting on it would wait on itself.
#[derive(Clone, Default)]
pub(crate) struct Outbox(Arc<Mutex<VecDeque<Effect>>>);

impl Outbox {
    pub(crate) fn push(&self, effect: impl Into<Effect>) {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back(effect.into());
    }

    /// Every effect asked for since the last take, oldest first; the lock is released before the
    /// caller applies them.
    pub(crate) fn take(&self) -> VecDeque<Effect> {
        std::mem::take(&mut *self.0.lock().unwrap_or_else(|e| e.into_inner()))
    }
}
