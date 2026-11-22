//! The one door plugin code goes through into the page: a session the app knows, and the page
//! checked to be the one the work was started for.
//! `inject_plugin` takes an `Admission`: no caller of it can skip these.

use super::jsrt::PassGuard;
use crate::app_state::{Ticket, exec_js_on_frame, with_state};
use cef::*;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

/// Proof that `ticket` was the current page of a known session. Never cloned, and nothing outside
/// this module can make one.
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
    /// The session or the page the work was started for is gone.
    Stale,
    /// No session holds a credential the app knows: TIDAL's page may hold a real token that
    /// nothing here can rotate, purge or recognise on its way out.
    NoSession,
}

impl Closed {
    pub(super) fn reason(&self) -> &'static str {
        match self {
            Closed::Stale => "The page reloaded before the plugin could start",
            Closed::NoSession => {
                "TIDAL's session is not known to the app; sign in again to use plugins"
            }
        }
    }
}

/// What the state alone must allow before any plugin code is sent: a session the app knows.
/// Shared with `install`, which asks it before the user is asked anything.
pub(super) fn standing(session_known: bool) -> Result<(), Closed> {
    if !session_known {
        return Err(Closed::NoSession);
    }
    Ok(())
}

/// Whether an admission may go, from one look at the state. Work for a page or session already
/// gone is stale before anything else: it is dropped quietly rather than announced.
fn entry(current: Ticket, asked: Ticket, standing: Result<(), Closed>) -> Result<(), Closed> {
    if current != asked {
        return Err(Closed::Stale);
    }
    standing
}

/// Admit plugin code for `ticket`. Under the pass lock.
pub(super) fn admit(_pass: &PassGuard, ticket: Ticket) -> Result<Admission, Closed> {
    with_state(|state| {
        entry(
            state.ticket(),
            ticket,
            standing(state.session_for_plugins()),
        )?;
        // Set before any plugin line can run, and only for the page still current: a stale pass
        // must not close the store-move window on a page whose own bundle has not asked yet.
        state.plugin_code_admitted = true;
        Ok(Admission { ticket })
    })
    .unwrap_or(Err(Closed::Stale))
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

/// Hand `js` to the page `ticket` names, checked on the UI thread right before it goes out.
/// Checked on a worker, a reload committing between the check and the frame lookup would land
/// the code in the new document; the UI thread orders the commit, the page bump and this task.
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
