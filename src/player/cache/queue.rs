use std::fs;
use std::sync::mpsc;
use std::sync::{LazyLock, Mutex};

use super::evict::locked;
use super::store::hold_moves_off;
use super::{AudioCache, CacheError, CacheState, Request};

type Run = Box<dyn FnOnce(&mut AudioCache) -> Result<(), CacheError> + Send>;

enum Change {
    Plain(Run),
    /// A Clear: the applier takes the move gate before the cache lock and runs the wipe with the
    /// lock free between steps, which a closure over `&mut AudioCache` could not do.
    Wipe,
    /// A new cap: the applier lowers it under the lock, then runs the eviction pass with the lock
    /// free between steps, which a closure over `&mut AudioCache` could not do.
    Resize(u64),
}

/// The reply a click waits on, owed once its change is applied: the state that change left. Dropped
/// unanswered by any path (a poisoned lock, no thread to apply it, a panic in the change), it fails
/// instead, as CEF keeps the query open until something answers and the page holds the click till then.
pub(crate) struct Completion(Option<Answer>);

type Answer = Box<dyn FnOnce(Result<CacheState, &'static str>) + Send>;

impl Completion {
    pub(crate) fn new(
        answer: impl FnOnce(Result<CacheState, &'static str>) + Send + 'static,
    ) -> Self {
        Self(Some(Box::new(answer)))
    }

    fn answer(mut self, state: CacheState) {
        if let Some(answer) = self.0.take() {
            answer(Ok(state));
        }
    }
}

impl Drop for Completion {
    fn drop(&mut self) {
        if let Some(answer) = self.0.take() {
            answer(Err("audio cache change not applied"));
        }
    }
}

type Queued = (Request, Change, Option<Completion>);

/// Apply what `rx` delivers, in order, against `slot`, until every sender is gone.
fn apply_changes(rx: mpsc::Receiver<Queued>, slot: &Mutex<AudioCache>) {
    for (request, change, completion) in rx {
        match change {
            Change::Plain(run) => apply_one(slot, request, completion, run),
            Change::Wipe => {
                // Gate first, cache lock second: a move in flight is waited out here with the lock
                // free for every lookup, and no other lands until the Clear is answered; the reply
                // carries the state the wipe left, a download's row included only if it came after.
                let moves_off = hold_moves_off();
                if slot.is_poisoned() {
                    crate::vprintln!("[CACHE]  Lock poisoned, {request:?} skipped");
                    continue;
                }
                let outcome = AudioCache::wipe(slot, &moves_off, &|p| fs::remove_file(p));
                apply_one(slot, request, completion, move |cache| {
                    // What the sweep freed no row accounted for: a Clear that completed has
                    // taken the stranded bytes.
                    if outcome.is_ok() {
                        cache.stranded = 0;
                    }
                    outcome
                });
                drop(moves_off);
            }
            Change::Resize(max_bytes) => {
                if slot.is_poisoned() {
                    crate::vprintln!("[CACHE]  Lock poisoned, {request:?} skipped");
                    continue;
                }
                {
                    let mut cache = locked(slot);
                    cache.max_bytes = max_bytes;
                    cache.changed();
                }
                let outcome = AudioCache::drain(slot, None, 0, &|p| fs::remove_file(p));
                // The last step of the pass, the settling and the answer are one section: the
                // reply is numbered after every snapshot a step of the pass let the publisher
                // take, and carries the reason the pass left, if it did.
                apply_one(slot, request, completion, move |cache| {
                    cache.finish_pass(outcome)
                });
            }
        }
    }
}

fn apply_one(
    slot: &Mutex<AudioCache>,
    request: Request,
    completion: Option<Completion>,
    run: impl FnOnce(&mut AudioCache) -> Result<(), CacheError>,
) {
    let Ok(mut cache) = slot.lock() else {
        crate::vprintln!("[CACHE]  Lock poisoned, {request:?} skipped");
        return;
    };
    // Held across all three: the publisher can only snapshot the outcome with its record, and
    // the reply is numbered after every snapshot taken before the change.
    let outcome = run(&mut cache);
    cache.settle(request, outcome);
    let reply = completion.map(|completion| (completion, cache.state()));
    drop(cache);
    if let Some((completion, state)) = reply {
        completion.answer(state);
    }
}

/// One thread applies the user's changes, in the order they were made. Racing for the lock on
/// threads of their own, a switch flipped off then on could land off. And none of it can run on
/// the UI thread its handlers live on: a clear or a lowered cap deletes files.
static CHANGES: LazyLock<mpsc::Sender<Queued>> = LazyLock::new(|| {
    let (tx, rx) = mpsc::channel::<Queued>();
    let applier = move || apply_changes(rx, &crate::state::AUDIO_CACHE);
    // Not `thread::spawn`: a panic here would poison this lock for the process lifetime.
    if let Err(e) = std::thread::Builder::new()
        .name("cache-changes".into())
        .spawn(applier)
    {
        crate::verr!("[CACHE]  No thread to apply settings changes: {e}");
    }
    tx
});

/// Queue a change from the settings or the menu; the page hears the result once it is applied,
/// and the click that asked for it, if any, through `completion`.
pub(crate) fn queue_change(
    request: Request,
    change: impl FnOnce(&mut AudioCache) -> Result<(), CacheError> + Send + 'static,
    completion: Option<Completion>,
) {
    send_change(request, Change::Plain(Box::new(change)), completion);
}

/// Queue a Clear. Apart from [`queue_change`] because the wipe needs the move gate taken before
/// the cache lock, which only the applier can arrange.
pub(crate) fn queue_clear(completion: Option<Completion>) {
    send_change(Request::Clear, Change::Wipe, completion);
}

/// Queue a new cap. Apart from [`queue_change`] because the pass that lowers the cache to it must
/// not hold the cache lock while it unlinks.
pub(crate) fn queue_resize(max_bytes: u64, completion: Option<Completion>) {
    send_change(Request::Resize, Change::Resize(max_bytes), completion);
}

fn send_change(request: Request, change: Change, completion: Option<Completion>) {
    if CHANGES.send((request, change, completion)).is_err() {
        crate::verr!("[CACHE]  {request:?} not applied: no thread to apply it");
    }
}
