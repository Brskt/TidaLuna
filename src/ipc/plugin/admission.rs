//! The one door plugin code goes through into the page: the real token rotated out of TIDAL's SDK,
//! its stored blob purged, and the page checked to be the one the work was started for.
//! `inject_plugin` takes an `Admission`: no caller of it can skip these.

use super::jsrt::PassGuard;
use crate::app_state::{Ticket, exec_js_on_frame, with_state};
use cef::*;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

/// Proof that the gate was open and the blob purged for `ticket`. Never cloned, and nothing
/// outside this module can make one.
pub(super) struct Admission {
    ticket: Ticket,
}

impl Admission {
    pub(super) fn ticket(&self) -> Ticket {
        self.ticket
    }
}

/// Why no plugin may run now.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Closed {
    /// The cold-boot refresh has not rotated the real token yet.
    Gate,
    /// The session or the page the work was started for is gone.
    Stale,
    /// The purge could not reach the page; the blob is still there.
    PurgeUndelivered,
}

impl Closed {
    pub(super) fn reason(&self) -> &'static str {
        match self {
            Closed::Gate => "The app is still signing in; try again in a moment",
            Closed::Stale => "The page reloaded before the plugin could start",
            Closed::PurgeUndelivered => "The page could not be prepared for plugins",
        }
    }
}

/// What an admission may do, from one look at the state: refuse, or go, purging first when the
/// blob is still due.
fn entry(gate_open: bool, current: Ticket, asked: Ticket, purge_due: bool) -> Result<bool, Closed> {
    if !gate_open {
        return Err(Closed::Gate);
    }
    if current != asked {
        return Err(Closed::Stale);
    }
    Ok(purge_due)
}

/// Admit plugin code for `ticket`, purging the SDK blob first if it is still due. Under the pass
/// lock. No second caller finds the purge flag taken and injects before the purge went out.
/// Blocks: from a blocking task only, never the UI thread nor under the state lock.
pub(super) fn admit(_pass: &PassGuard, ticket: Ticket) -> Result<Admission, Closed> {
    let purge = with_state(|state| {
        let purge = entry(
            state.proactive_refresh_done,
            state.ticket(),
            ticket,
            state.needs_blob_purge,
        )?;
        state.needs_blob_purge = false;
        Ok(purge)
    })
    .unwrap_or(Err(Closed::Stale))?;
    if purge {
        // Named one by one: a browser closing has no frame and is no page that failed to prepare.
        let refused = match eval_for(ticket, super::jsrt::JS_PURGE_SDK_BLOB.to_string()) {
            Delivery::Ran => None,
            Delivery::Stale | Delivery::NoFrame => Some(Closed::Stale),
            Delivery::Undelivered => Some(Closed::PurgeUndelivered),
        };
        if let Some(refused) = refused {
            // Still due: the next admission tries again rather than leave the blob for good.
            with_state(|state| state.needs_blob_purge = true);
            return Err(refused);
        }
        crate::vprintln!("[AUTH]   Purged SDK auth blob from localStorage (pre-plugin)");
    }
    Ok(Admission { ticket })
}

/// What became of a piece of JS handed to the page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Delivery {
    /// Sent to the main frame of the page it was meant for.
    Ran,
    /// That page or session is gone; nothing was sent.
    Stale,
    /// The page has no main frame; nothing was sent.
    NoFrame,
    /// The UI thread never took it, and it never will; nothing was sent.
    Undelivered,
}

/// Run a plugin's code in the page its admission names, behind the page's own stamp as well.
pub(super) fn eval_plugin(admission: &Admission, js: &str) -> Delivery {
    let page = admission.ticket.page;
    eval_for(
        admission.ticket,
        format!("if(window.__LUNAR_PAGE__==={page}){{\n{js}\n}}"),
    )
}

/// The statement stamping a new main document with its generation, run before anything else in
/// it. A stale eval that the browser could not stop reaches a page that refuses it.
pub(crate) fn page_stamp(generation: u64) -> String {
    format!("Object.defineProperty(window,'__LUNAR_PAGE__',{{value:{generation}}});")
}

/// How long a worker waits for the UI thread to take its eval. Generous: the UI thread has real
/// multi-second stalls, and giving up abandons a whole load pass.
const EVAL_WAIT: Duration = Duration::from_secs(15);

const PENDING: u8 = 0;
const CLAIMED: u8 = 1;
const CANCELLED: u8 = 2;

/// The race between the UI thread running an eval and the worker giving up on it, settled once.
#[derive(Default)]
pub(super) struct Claim(AtomicU8);

impl Claim {
    /// The UI thread's side: true when it may run the eval.
    pub(super) fn claim(&self) -> bool {
        self.0
            .compare_exchange(PENDING, CLAIMED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// The worker's side: true when the eval is now guaranteed never to run.
    pub(super) fn cancel(&self) -> bool {
        self.0
            .compare_exchange(PENDING, CANCELLED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
}

/// Hand `js` to the page `ticket` names, checked on the UI thread right before it goes out. Checked
/// here instead, a reload committing between the check and the frame lookup would land the code in
/// the new document; on the UI thread the commit, the page bump and this task are ordered.
fn eval_for(ticket: Ticket, js: String) -> Delivery {
    let claim = Arc::new(Claim::default());
    let (tx, rx) = mpsc::channel();
    let mut task = CheckedEval::new(js, ticket, Arc::clone(&claim), Arc::new(Mutex::new(tx)));
    if post_task(ThreadId::UI, Some(&mut task)) == 0 {
        return Delivery::Undelivered;
    }
    // The task holds the only sender: a task dropped unrun disconnects this at once.
    drop(task);
    match rx.recv_timeout(EVAL_WAIT) {
        Ok(delivery) => delivery,
        Err(mpsc::RecvTimeoutError::Disconnected) => Delivery::Undelivered,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            if claim.cancel() {
                crate::verr!("[PLUGIN] The UI thread did not take an eval within {EVAL_WAIT:?}");
                return Delivery::Undelivered;
            }
            // Claimed already: the task is running and answers without blocking.
            rx.recv().unwrap_or(Delivery::Undelivered)
        }
    }
}

/// On the UI thread: the page check and the send, with no commit possible in between.
fn deliver(ticket: Ticket, js: &str) -> Delivery {
    let browser =
        match with_state(|state| (state.ticket() == ticket).then(|| state.browser.clone())) {
            Some(Some(Some(browser))) => browser,
            // An unreadable state is no proof the page is still the one asked for.
            Some(None) | None => return Delivery::Stale,
            Some(Some(None)) => return Delivery::NoFrame,
        };
    match browser.main_frame() {
        Some(frame) => {
            exec_js_on_frame(&frame, js);
            Delivery::Ran
        }
        None => Delivery::NoFrame,
    }
}

wrap_task! {
    struct CheckedEval {
        js: String,
        ticket: Ticket,
        claim: Arc<Claim>,
        reply: Arc<Mutex<mpsc::Sender<Delivery>>>,
    }
    impl Task {
        fn execute(&self) {
            if !self.claim.claim() {
                return;
            }
            let delivery = deliver(self.ticket, &self.js);
            let _ = self.reply.lock().unwrap_or_else(|e| e.into_inner()).send(delivery);
        }
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/ipc/plugin/admission.rs"]
mod tests;
