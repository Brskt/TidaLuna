//! Plugin-store fetch proxy.
//!
//! The plugin store fetches each `store.json` from a GitHub `releases/download` URL. GitHub
//! 302-redirects to a signed `release-assets.githubusercontent.com` CDN URL, and Chromium's
//! network stack rejects that redirect target with `net::ERR_INVALID_ARGUMENT` (an unguarded,
//! unconfigurable header-safety check). reqwest follows the same redirect fine, so we take the
//! request over with a `ResourceHandler` and serve the bytes ourselves, transparent to the
//! renderer. Non-GitHub store hosts keep going through CEF natively.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use cef::*;
use futures_util::StreamExt;

use crate::ui::token_filter::userfree_to_string;

/// Overall budget for a single store fetch (connect + redirect + body).
const STORE_FETCH_TIMEOUT_SECS: u64 = 15;
/// Body cap: a `store.json` is a few KB; this only bounds a hostile/oversized source.
const MAX_STORE_BYTES: usize = 16 * 1024 * 1024;

/// True for any `store.json` fetch (any host). The host is not restricted -- the safety gate
/// is the JSON validation in `fetch_store`, not an allow-list.
fn should_proxy(url: &str) -> bool {
    (url.starts_with("https://") || url.starts_with("http://")) && url.ends_with("/store.json")
}

/// The store-proxy request handler for a matching GitHub store sub-resource fetch, else None.
/// Wired into BOTH dispatches: the browser-level one (`client.rs`) and the context-level one
/// (`csp_filter.rs`) -- TIDAL's service worker routes some fetches browser-less, and those
/// reach only the context handler.
pub(super) fn intercept(
    url: &str,
    is_navigation: ::std::os::raw::c_int,
    is_download: ::std::os::raw::c_int,
) -> Option<ResourceRequestHandler> {
    if is_navigation == 0 && is_download == 0 && should_proxy(url) {
        crate::vprintln3!("[STORE]  intercept {}", crate::util::truncate_str(url, 90));
        Some(StoreProxyRequestHandler::new())
    } else {
        None
    }
}

/// Shared state: a spawned fetch task fills `body`/`status`, then the CEF IO-thread callbacks
/// (`response_headers`, `read`) drain it. The fetch runs off-thread and `cancel` may race it,
/// hence the `Mutex`.
#[derive(Default)]
struct ProxyState {
    body: Vec<u8>,
    offset: usize,
    /// HTTP status to report; 0 means the fetch failed entirely -> reported as 502.
    status: i32,
    /// Set when CEF calls `cancel`; the deferred `cont()` checks it (on the IO thread) so it
    /// never resumes a request CEF already aborted.
    cancelled: bool,
}

/// Fetch `url` via reqwest (following redirects, size/time capped) and return the body ONLY
/// if it is a conforming store manifest -- a JSON object carrying a `plugins` array. `None`
/// on any failure (transport, timeout, oversized) or if the response is not a store manifest
/// (an HTML error page, a binary, a `.exe`, or unrelated JSON). Any link is allowed; this
/// content check is the only gate, so only real store data is served and nothing is executed.
async fn fetch_store(url: &str) -> Option<Vec<u8>> {
    let body = tokio::time::timeout(Duration::from_secs(STORE_FETCH_TIMEOUT_SECS), async {
        let resp = crate::state::HTTP_CLIENT.get(url).send().await.ok()?;
        let mut stream = resp.bytes_stream();
        let mut buf = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.ok()?;
            if buf.len() + chunk.len() > MAX_STORE_BYTES {
                return None;
            }
            buf.extend_from_slice(&chunk);
        }
        Some(buf)
    })
    .await
    .ok()
    .flatten()?;

    match serde_json::from_slice::<serde_json::Value>(&body) {
        Ok(v) if v.get("plugins").is_some_and(|p| p.is_array()) => Some(body),
        _ => None,
    }
}

// Hands back the body-serving `StoreProxyHandler` for a matched store request.
wrap_resource_request_handler! {
    pub(super) struct StoreProxyRequestHandler;

    impl ResourceRequestHandler {
        // The trait default returns RV_CANCEL, which kills the request before
        // `resource_handler` can run -- so we must explicitly continue.
        fn on_before_resource_load(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _request: Option<&mut Request>,
            _callback: Option<&mut Callback>,
        ) -> ReturnValue {
            ReturnValue::CONTINUE
        }

        fn resource_handler(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _request: Option<&mut Request>,
        ) -> Option<ResourceHandler> {
            Some(StoreProxyHandler::new(Arc::new(Mutex::new(ProxyState::default()))))
        }
    }
}

// Serves the reqwest-fetched body, bypassing Chromium's network stack.
wrap_resource_handler! {
    pub(super) struct StoreProxyHandler {
        state: Arc<Mutex<ProxyState>>,
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
            // Async path: return now and finish via `callback` once the fetch resolves, so this
            // CEF IO thread is never parked on the (up to 15s) fetch. Resource-handler callbacks
            // are sequenced on a shared CEF thread, so blocking here stalls all resource loads.
            if let Some(hr) = handle_request {
                *hr = 0;
            }
            let Some(callback) = callback else {
                return 0;
            };
            let callback = callback.clone(); // refcounted: stays alive across the await + hand-off
            let state = self.state.clone();
            crate::state::rt_handle().spawn(async move {
                let body = fetch_store(&url).await;
                match &body {
                    Some(b) => crate::vprintln!(
                        "[STORE]  served {} ({} bytes)",
                        crate::util::truncate_str(&url, 90),
                        b.len()
                    ),
                    None => crate::vprintln!(
                        "[STORE]  rejected {} (fetch failed or not JSON)",
                        crate::util::truncate_str(&url, 90)
                    ),
                }
                {
                    let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
                    match body {
                        Some(b) => {
                            st.status = 200;
                            st.body = b;
                        }
                        None => st.status = 0,
                    }
                }
                // Resume CEF on its own IO thread (Continue from a tokio worker is the
                // documented crash class). Runs exactly once, so the request never hangs.
                let mut task = StoreContinueTask::new(callback, state);
                post_task(ThreadId::IO, Some(&mut task));
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
            // status 0 == reqwest failed outright -> 502 so the store UI sees `response.ok`
            // false instead of hanging. Do NOT touch `redirect_url` (that re-enters the
            // failing Chromium path).
            let status = if st.status == 0 { 502 } else { st.status };
            resp.set_status(status);
            resp.set_mime_type(Some(&CefString::from("application/json")));
            // Chromium still enforces CORS on this served cross-origin response (the
            // store fetches GitHub from the desktop.tidal.com origin); allow it. A simple
            // GET needs no preflight, so this single header is enough.
            resp.set_header_by_name(
                Some(&CefString::from("Access-Control-Allow-Origin")),
                Some(&CefString::from("*")),
                1,
            );
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
            // SAFETY: CEF guarantees `data_out` is writable for at least `bytes_to_read`
            // bytes for the duration of this call; we copy `n <= bytes_to_read` and return.
            unsafe {
                std::ptr::copy_nonoverlapping(st.body.as_ptr().add(st.offset), data_out, n);
            }
            st.offset += n;
            *br = n as ::std::os::raw::c_int;
            1
        }

        fn cancel(&self) {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            // Mark cancelled so the deferred `cont()` task (also on the IO thread) skips a
            // request CEF has already aborted.
            st.cancelled = true;
            st.body = Vec::new();
            st.offset = 0;
        }
    }
}

// Resumes the CEF request once the fetch has populated the shared state. Skips a request CEF
// already cancelled (checked on the IO thread, where it serialises with `cancel()`).
wrap_task! {
    struct StoreContinueTask {
        callback: Callback,
        state: Arc<Mutex<ProxyState>>,
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
