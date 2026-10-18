//! Serves decoded animated-cover frames to the page over a scheme we own.
//!
//! Frames cross as raw I420 rather than RGB: colour conversion belongs on the GPU, and I420 is
//! half RGB's bytes. Neither `cefQuery` nor `execute_java_script` can carry them, both
//! marshalling through UTF-16 text.
//!
//! A registered scheme rather than a synthetic `https://` path: `store_proxy` and `luna_modules`
//! both bet that their path never collides with a real TIDAL request, and a scheme we own does
//! not take that bet.

use std::sync::{Arc, Mutex};

use cef::*;

use crate::ui::nav::PageKind;
use crate::ui::token_filter::userfree_to_string;
use crate::video::cover::{CoverFrame, CoverInterest, CoverOutcome, canonical_cover_url};

pub(crate) const COVER_SCHEME: &str = "luna-cover";

/// Magic naming the wire format: a page handed a foreign body says so instead of painting
/// noise.
const MAGIC: &[u8; 4] = b"LVC1";

/// Bytes before the first frame record: the magic, then width, height and frame count as u32.
const HEADER_LEN: usize = 16;

/// Bytes per frame record in the header.
const RECORD_LEN: usize = 32;

/// Pull the original artwork URL back out of a scheme URL.
///
/// The source travels as a percent-encoded query parameter rather than as a path, so that the
/// `https://` URL's own slashes and query survive the round trip untouched.
fn source_url(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    parsed
        .query_pairs()
        .find(|(k, _)| k == "src")
        .map(|(_, v)| v.into_owned())
}

/// Lay the frames out in the order `decodeCoverBuffer` (frontend `cover-canvas.ts`) reads them.
///
/// Takes the frames by value: each frame's planes are freed as soon as they are copied, where
/// borrowing would hold the decoded cover and its body at once in the browser process. The
/// `with_capacity` below takes RAM as it fills; peak is one cover plus the frame in hand.
fn encode_frames(frames: Vec<CoverFrame>) -> Vec<u8> {
    let (width, height) = frames
        .first()
        .map(|f| (f.width as u32, f.height as u32))
        .unwrap_or((0, 0));

    let planes: usize = frames
        .iter()
        .map(|f| f.y.len() + f.u.len() + f.v.len())
        .sum();
    let mut out = Vec::with_capacity(HEADER_LEN + frames.len() * RECORD_LEN + planes);

    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&width.to_le_bytes());
    out.extend_from_slice(&height.to_le_bytes());
    out.extend_from_slice(&(frames.len() as u32).to_le_bytes());

    for f in &frames {
        out.extend_from_slice(&f.pts_micros.to_le_bytes());
        out.extend_from_slice(&(f.y_stride as u32).to_le_bytes());
        out.extend_from_slice(&(f.u_stride as u32).to_le_bytes());
        out.extend_from_slice(&(f.v_stride as u32).to_le_bytes());
        out.extend_from_slice(&(f.y.len() as u32).to_le_bytes());
        out.extend_from_slice(&(f.u.len() as u32).to_le_bytes());
        out.extend_from_slice(&(f.v.len() as u32).to_le_bytes());
    }
    for f in frames {
        out.extend_from_slice(&f.y);
        out.extend_from_slice(&f.u);
        out.extend_from_slice(&f.v);
    }
    out
}

#[derive(Default)]
struct CoverState {
    /// Shared, because one decoded cover can answer several requests at once: it is the body of a
    /// job, not of a request, and copying it per requester would undo the sharing that produced it.
    body: Arc<Vec<u8>>,
    offset: usize,
    status: u16,
    reason: String,
    cancelled: bool,
    /// This request's share of its job's interest, given back exactly once when CEF cancels.
    ///
    /// `cancelled` lives behind this mutex, which `read()` takes on the CEF IO thread; the fetch
    /// and the decode run elsewhere and must not contend with it, least of all from inside a
    /// per-frame loop. The count beside it is read for free wherever the work happens to be.
    interest: Option<Arc<CoverInterest>>,
}

/// One finished answer, as every request that asked for the same cover receives it.
struct CoverResponse {
    status: u16,
    reason: String,
    body: Arc<Vec<u8>>,
}

/// A cover being fetched and decoded right now, and the requesters it answers.
struct CoverJob {
    /// A receiver only, cloned to join. The sender lives in the job's task alone: whatever ends
    /// that task drops it, which ends every waiter's wait and marks the entry dead to joiners.
    result: CoverAnswer,
    interest: Arc<CoverInterest>,
}

/// The covers in flight, keyed by [`canonical_cover_url`], never by what the page asked for
/// (two requests for one cover routinely name different variants).
///
/// Single-flight: a cover is the costliest thing this module produces (up to
/// `MAX_DECODED_BYTES` of decode plus the encoded body), and TIDAL renders one in several places
/// at once (now-playing artwork, album header), each its own request. Same shape as
/// `PENDING_TRUST` in `ipc::plugin::native`: a `watch` per key, a receiver per joiner, the
/// value still readable from a receiver taken before the key was retired.
static COVER_JOBS: std::sync::LazyLock<Mutex<std::collections::HashMap<String, CoverJob>>> =
    std::sync::LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));

/// Run one cover to its answer, publish it to everyone waiting, and retire the key.
///
/// Retired and published under one lock: a receiver taken earlier still reads the value from its
/// own handle once the sender is gone, so removal only stops LATER requests from joining work that
/// is already over. Nothing here is a cache: the page keeps the decoded cover it painted, and a
/// request arriving after this starts the work again.
fn spawn_cover_job(
    key: String,
    interest: Arc<CoverInterest>,
    result: tokio::sync::watch::Sender<Option<Arc<CoverResponse>>>,
) {
    let load = {
        let key = key.clone();
        let interest = interest.clone();
        // The encode runs inside the load (`load_cover`'s `finish`), leaving only a send inside the
        // lock of `run_cover_job`: `open()` takes the same lock on the CEF IO thread.
        async move {
            crate::video::cover::load_cover(&key, interest, |frames| {
                crate::vprintln!("[VIDEO]   Cover decoded ({} frames)", frames.len());
                encode_frames(frames)
            })
            .await
        }
    };
    crate::state::rt_handle().spawn(run_cover_job(key, interest, result, load));
}

/// The job behind [`spawn_cover_job`], with its load handed in: the tests drive it with a load of
/// their own, where the real one wants the network and a decoder.
async fn run_cover_job(
    key: String,
    mine: Arc<CoverInterest>,
    result: tokio::sync::watch::Sender<Option<Arc<CoverResponse>>>,
    load: impl std::future::Future<Output = CoverOutcome<Vec<u8>>>,
) {
    let outcome = load.await;
    let (status, reason, body) = match outcome {
        CoverOutcome::Decoded(body) => (200, String::new(), body),
        // Not an error: the page keeps the still image and is told why.
        CoverOutcome::NoCodec(st) => {
            crate::vprintln!("[VIDEO]   Cover without a codec: {}", st.reason());
            (503, st.reason().to_string(), Vec::new())
        }
        // The cause stays in the log: a loader message names the codec's path on disk.
        CoverOutcome::Failed(cause) => {
            crate::verr!("[VIDEO]   Cover failed: {cause}");
            (502, "the cover could not be loaded".to_string(), Vec::new())
        }
        CoverOutcome::Abandoned => {
            crate::vprintln!("[VIDEO]   Cover abandoned before it was needed");
            // nginx's "client closed request".
            (499, "the request was abandoned".to_string(), Vec::new())
        }
    };
    let answer = Arc::new(CoverResponse {
        status,
        reason,
        body: Arc::new(body),
    });

    let mut jobs = COVER_JOBS.lock().unwrap_or_else(|e| e.into_inner());
    // Only if the entry is still OURS. A job whose last requester left is replaced in the
    // table by the next request rather than joined, and answering on that newcomer's channel
    // would hand it a result computed for somebody else.
    let still_ours = jobs
        .get(&key)
        .is_some_and(|job| Arc::ptr_eq(&job.interest, &mine));
    if still_ours && jobs.remove(&key).is_some() {
        let _ = result.send(Some(answer));
    }
}

/// What one requester waits on: its job's answer, once there is one.
type CoverAnswer = tokio::sync::watch::Receiver<Option<Arc<CoverResponse>>>;

/// Join the job already running for `key`, or register a new one and hand it to `start`.
///
/// Joined or started under one lock: the decision cannot straddle another request's. `start` is
/// [`spawn_cover_job`] outside the tests.
fn join_or_start(
    key: String,
    start: impl FnOnce(
        String,
        Arc<CoverInterest>,
        tokio::sync::watch::Sender<Option<Arc<CoverResponse>>>,
    ),
) -> (CoverAnswer, Arc<CoverInterest>) {
    let mut jobs = COVER_JOBS.lock().unwrap_or_else(|e| e.into_inner());
    let joined = jobs.get(&key).and_then(|job| {
        // A closed channel is a job whose task ended without publishing: it panicked, or its
        // runtime dropped it. Joining it would wait on nothing, and the entry is replaced below.
        let alive = job.result.has_changed().is_ok();
        (alive && job.interest.try_join()).then(|| (job.result.clone(), job.interest.clone()))
    });
    match joined {
        Some(pair) => pair,
        None => {
            let interest = Arc::new(CoverInterest::new());
            let (tx, rx) = tokio::sync::watch::channel(None);
            jobs.insert(
                key.clone(),
                CoverJob {
                    result: rx.clone(),
                    interest: interest.clone(),
                },
            );
            start(key, interest.clone(), tx);
            (rx, interest)
        }
    }
}

/// Wait for the job's answer, or name its absence should the job go without one.
async fn await_answer(mut rx: CoverAnswer) -> Arc<CoverResponse> {
    // The current value first: the job may have finished between the join and this task
    // being polled, and a latecomer that only waited for a CHANGE would wait for a send that
    // already happened.
    let answer = loop {
        let seen = rx.borrow().clone();
        if let Some(found) = seen {
            break Some(found);
        }
        if rx.changed().await.is_err() {
            break rx.borrow().clone();
        }
    };
    answer.unwrap_or_else(|| {
        // The sender drops unsent when the job's task ends without publishing: it panicked,
        // its runtime dropped it, or it finished after a newcomer had replaced it.
        Arc::new(CoverResponse {
            status: 502,
            reason: "the cover job ended without an answer".to_string(),
            body: Arc::new(Vec::new()),
        })
    })
}

/// Why a requester is refused, or `None` to serve it.
///
/// The work behind this scheme runs in the browser process, and any document loaded in the main
/// browser can issue the request: an External iframe or page would otherwise make us download and
/// decode on its behalf. Only TIDAL's own pages may start it, by the rule privileged IPC applies.
/// The frame is CEF's, never the page's word; a frameless request (a service worker's) is refused.
fn requester_refusal(frame_url: Option<&str>) -> Option<&'static str> {
    match frame_url {
        None => Some("the cover request has no requesting frame"),
        Some(url) if !PageKind::classify(url).is_trusted() => {
            Some("the requesting frame is not a TIDAL page")
        }
        Some(_) => None,
    }
}

/// Hand one request its answer and resume CEF on its IO thread, where `cancel()` also runs:
/// the `cancelled` check before `cont()` cannot then race a cancel.
fn deliver(state: Arc<Mutex<CoverState>>, callback: Callback, answer: &CoverResponse) {
    {
        let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
        st.status = answer.status;
        st.reason.clone_from(&answer.reason);
        st.body = answer.body.clone();
    }
    let mut task = CoverContinueTask::new(callback, state);
    post_task(ThreadId::IO, Some(&mut task));
}

wrap_resource_handler! {
    pub(super) struct CoverHandler {
        state: Arc<Mutex<CoverState>>,
        // Decided in `create()`, where CEF names the requesting frame.
        refusal: Option<&'static str>,
    }

    impl ResourceHandler {
        fn open(
            &self,
            request: Option<&mut Request>,
            handle_request: Option<&mut ::std::os::raw::c_int>,
            callback: Option<&mut Callback>,
        ) -> ::std::os::raw::c_int {
            let url = request
                .as_ref()
                .map(|r| userfree_to_string(&r.url()))
                .unwrap_or_default();
            // Async path: return now and finish via `callback` once the decode resolves. This
            // CEF IO thread is shared by every resource load, and blocking it stalls all of them.
            if let Some(hr) = handle_request {
                *hr = 0;
            }
            let Some(callback) = callback else {
                return 0;
            };
            let callback = callback.clone();
            let state = self.state.clone();

            // Before anything that costs: no job is joined or started for a refused requester.
            if let Some(reason) = self.refusal {
                deliver(
                    state,
                    callback,
                    &CoverResponse {
                        status: 403,
                        reason: reason.to_string(),
                        body: Arc::new(Vec::new()),
                    },
                );
                return 1;
            }

            let Some(src) = source_url(&url) else {
                deliver(
                    state,
                    callback,
                    &CoverResponse {
                        status: 502,
                        reason: "cover request carries no src".to_string(),
                        body: Arc::new(Vec::new()),
                    },
                );
                return 1;
            };

            let (rx, interest) = join_or_start(canonical_cover_url(&src), spawn_cover_job);
            // Recorded before the wait, so a `cancel()` arriving at any point after this finds a
            // share to give back. Nothing else ever takes it.
            {
                let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
                st.interest = Some(interest);
            }

            crate::state::rt_handle().spawn(async move {
                let answer = await_answer(rx).await;
                deliver(state, callback, &answer);
            });
            1
        }

        fn response_headers(
            &self,
            response: Option<&mut Response>,
            response_length: Option<&mut i64>,
            _redirect_url: Option<&mut CefString>,
        ) {
            let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let Some(resp) = response else {
                return;
            };
            resp.set_status(if st.status == 0 { 502 } else { st.status as i32 });
            resp.set_mime_type(Some(&CefString::from("application/octet-stream")));
            resp.set_header_by_name(
                Some(&CefString::from("Access-Control-Allow-Origin")),
                Some(&CefString::from("*")),
                1,
            );
            // Exposed as well as allowed: a standard scheme has its own origin, which makes the
            // page's fetch cross-origin, and CORS shows it only safelisted headers. Without this,
            // every refusal would reach the page as a bare status code.
            resp.set_header_by_name(
                Some(&CefString::from("Access-Control-Expose-Headers")),
                Some(&CefString::from("X-Luna-Cover-Reason")),
                1,
            );
            // The named reason travels as a header so the page can say why a cover stayed still
            // without a second round trip.
            if !st.reason.is_empty() {
                resp.set_header_by_name(
                    Some(&CefString::from("X-Luna-Cover-Reason")),
                    Some(&CefString::from(st.reason.as_str())),
                    1,
                );
            }
            if let Some(len) = response_length {
                *len = st.body.len() as i64;
            }
        }

        fn read(
            &self,
            data_out: *mut u8,
            bytes_to_read: ::std::os::raw::c_int,
            bytes_read: Option<&mut ::std::os::raw::c_int>,
            _callback: Option<&mut ResourceReadCallback>,
        ) -> ::std::os::raw::c_int {
            let Some(br) = bytes_read else {
                return 0;
            };
            if bytes_to_read <= 0 || data_out.is_null() {
                *br = 0;
                return 0;
            }
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let remaining = st.body.len().saturating_sub(st.offset);
            if remaining == 0 {
                *br = 0;
                return 0; // EOF
            }
            let n = remaining.min(bytes_to_read as usize);
            // SAFETY: CEF guarantees `data_out` is writable for at least `bytes_to_read` bytes
            // for the duration of this call; we copy `n <= bytes_to_read` and return.
            unsafe {
                std::ptr::copy_nonoverlapping(st.body.as_ptr().add(st.offset), data_out, n);
            }
            st.offset += n;
            *br = n as ::std::os::raw::c_int;
            1
        }

        fn cancel(&self) {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            st.cancelled = true;
            // ONE requester leaving, not the work stopping. The job goes on for whoever else is
            // still waiting on the same cover, and stops only when this was the last share. Taken
            // rather than released in place, so a second `cancel()` on the same handler cannot
            // give back a share twice.
            if let Some(interest) = st.interest.take() {
                interest.release();
            }
            st.body = Arc::new(Vec::new());
            st.offset = 0;
        }
    }
}

wrap_task! {
    struct CoverContinueTask {
        callback: Callback,
        state: Arc<Mutex<CoverState>>,
    }
    impl Task {
        fn execute(&self) {
            if self.state.lock().unwrap_or_else(|e| e.into_inner()).cancelled {
                return;
            }
            self.callback.cont();
        }
    }
}

wrap_scheme_handler_factory! {
    pub(super) struct CoverSchemeFactory {}

    impl SchemeHandlerFactory {
        fn create(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            _scheme_name: Option<&CefString>,
            _request: Option<&mut Request>,
        ) -> Option<ResourceHandler> {
            let frame_url = frame.map(|f| userfree_to_string(&f.url()));
            let refusal = requester_refusal(frame_url.as_deref());
            if let Some(reason) = refusal {
                let from = frame_url
                    .as_deref()
                    .map_or_else(|| "no frame".to_string(), crate::util::refused_host);
                crate::vprintln!("[VIDEO]   Refused a cover request from {from}: {reason}");
            }
            Some(CoverHandler::new(
                Arc::new(Mutex::new(CoverState::default())),
                refusal,
            ))
        }
    }
}

/// Register the factory. Browser process only: `on_register_custom_schemes` runs in every
/// process, but a scheme handler factory only means anything where the network stack lives.
pub(crate) fn register() {
    let scheme = CefString::from(COVER_SCHEME);
    let mut factory = CoverSchemeFactory::new();
    if register_scheme_handler_factory(Some(&scheme), None, Some(&mut factory)) == 0 {
        crate::verr!("[VIDEO]   Registering the cover scheme handler failed");
    }
}

#[cfg(test)]
#[path = "../../tests/unit/ui/cover_scheme.rs"]
mod tests;
