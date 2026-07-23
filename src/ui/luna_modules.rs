//! Serves Luna's own plugin ES modules (`luna-ui.mjs`, `luna-dev.mjs`) to the renderer.
//!
//! Baked into the binary (`include_bytes!`) and served on a synthetic same-origin `/__luna__/*.mjs`
//! path so the loader can `import()` them, instead of `bundle.js` carrying them as escaped string
//! literals. Same `ResourceHandler` pattern as `store_proxy`, but synchronous (bytes are resident,
//! so no fetch/callback/`Task`).

use std::sync::{Arc, Mutex};

use cef::*;

static LUNA_UI_MJS: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/luna-ui.mjs"));
static LUNA_DEV_MJS: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/luna-dev.mjs"));

/// Map a synthetic `/__luna__/*.mjs` path to its baked bytes (any host; query/fragment ignored).
fn resolve(url: &str) -> Option<&'static [u8]> {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    if path.ends_with("/__luna__/ui.mjs") {
        Some(LUNA_UI_MJS)
    } else if path.ends_with("/__luna__/dev.mjs") {
        Some(LUNA_DEV_MJS)
    } else {
        None
    }
}

/// The module-serving request handler for a matched `/__luna__/*.mjs` fetch, else None. Wired into
/// BOTH dispatches (browser-level `client.rs` and context-level `csp_filter.rs`), like `store_proxy`
/// -- the service-worker-routed fetch reaches only the context handler.
pub(super) fn intercept(
    url: &str,
    is_navigation: ::std::os::raw::c_int,
    is_download: ::std::os::raw::c_int,
) -> Option<ResourceRequestHandler> {
    if is_navigation == 0
        && is_download == 0
        && let Some(bytes) = resolve(url)
    {
        return Some(LunaModuleRequestHandler::new(bytes));
    }
    None
}

/// Serving cursor over the resident slice (`read` advances `offset` across calls).
struct ModState {
    bytes: &'static [u8],
    offset: usize,
}

wrap_resource_request_handler! {
    pub(super) struct LunaModuleRequestHandler {
        bytes: &'static [u8],
    }

    impl ResourceRequestHandler {
        // The trait default returns RV_CANCEL, which kills the request before
        // `resource_handler` runs -- continue so our handler serves it.
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
            Some(LunaModuleHandler::new(Arc::new(Mutex::new(ModState {
                bytes: self.bytes,
                offset: 0,
            }))))
        }
    }
}

wrap_resource_handler! {
    pub(super) struct LunaModuleHandler {
        state: Arc<Mutex<ModState>>,
    }

    impl ResourceHandler {
        // Synchronous: the bytes are resident (`include_bytes!`), so signal handled-now and let
        // response_headers/read serve straight from the slice -- no async fetch/callback needed.
        fn open(
            &self,
            _request: Option<&mut Request>,
            handle_request: Option<&mut ::std::os::raw::c_int>,
            _callback: Option<&mut Callback>,
        ) -> ::std::os::raw::c_int {
            if let Some(hr) = handle_request {
                *hr = 1;
            }
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
            resp.set_status(200);
            // Must be a JS MIME for Chromium's module loader to accept an import() target.
            resp.set_mime_type(Some(&CefString::from("text/javascript")));
            // Served fresh from the binary; never cache, so a rebuilt module is never shadowed by
            // a stale copy at the stable path.
            resp.set_header_by_name(
                Some(&CefString::from("Cache-Control")),
                Some(&CefString::from("no-store")),
                1,
            );
            // The injected bundle imports these from an opaque (about:blank) origin, so the module
            // fetch is CORS-checked even though it hits the same host; allow it (like store_proxy).
            resp.set_header_by_name(
                Some(&CefString::from("Access-Control-Allow-Origin")),
                Some(&CefString::from("*")),
                1,
            );
            if let Some(len) = response_length {
                *len = st.bytes.len() as i64;
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
            let remaining = st.bytes.len().saturating_sub(st.offset);
            if remaining == 0 {
                *br = 0;
                return 0; // EOF
            }
            let n = remaining.min(bytes_to_read as usize);
            // SAFETY: CEF guarantees `data_out` is writable for at least `bytes_to_read` bytes for
            // this call; we copy `n <= bytes_to_read` from the resident slice and advance the offset.
            unsafe {
                std::ptr::copy_nonoverlapping(st.bytes.as_ptr().add(st.offset), data_out, n);
            }
            st.offset += n;
            *br = n as ::std::os::raw::c_int;
            1
        }

        fn cancel(&self) {}
    }
}
