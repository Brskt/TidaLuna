use cef::*;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::ui::buffering_filter::{FilterOutcome, force_identity_encoding, new_buffering_filter};
use crate::ui::nav::RequestUrl;

mod request;
mod response;

use request::{capture_client_id, inject_refresh_token, rewrite_authorization_header};
pub(crate) use request::{resolve_opaque_access, resolve_opaque_refresh};
use response::{ProcessResult, process_token_response};

/// Convert a CefStringUserfree to String without the crate's eprintln on null.
pub(crate) fn userfree_to_string(userfree: &CefStringUserfreeUtf16) -> String {
    let raw: Option<&cef::sys::_cef_string_utf16_t> = userfree.into();
    if raw.is_none() {
        return String::new();
    }
    format!("{}", CefString::from(userfree))
}

const OPAQUE_PREFIX: &str = "luna_";

/// Hosts where a missing Authorization header should be auto-filled with the bearer token.
/// Subset of should_rewrite_token - excludes telemetry/DRM hosts that don't need OAuth.
pub(crate) fn needs_auto_injection(url: &RequestUrl) -> bool {
    let Some(parsed) = url.parsed() else {
        return false;
    };
    if parsed.scheme() != "https" {
        return false;
    }

    crate::ui::nav::is_tidal_api_host(parsed.host_str().unwrap_or(""))
}

pub(crate) fn should_rewrite_token(url: &RequestUrl) -> bool {
    let Some(parsed) = url.parsed() else {
        return false;
    };
    if parsed.scheme() != "https" {
        return false;
    }
    let host = parsed.host_str().unwrap_or("");
    crate::ui::nav::is_tidal_api_host(host)
        || matches!(host, "login.tidal.com" | "auth.tidal.com")
        || (host == "fp.fa.tidal.com" && parsed.path().starts_with("/license"))
        || (host.starts_with("event-collector.") && host.ends_with(".tidalhi.fi"))
}

/// Opaque `luna_*` token nonce, or `None` if the system RNG is unavailable.
/// Callers generate this off the AppState lock and fail closed on `None`.
pub(crate) fn generate_opaque() -> Option<String> {
    generate_opaque_with(|buf| match getrandom::fill(buf) {
        Ok(()) => true,
        Err(e) => {
            crate::vprintln!("[token_filter] opaque entropy failure: {e}");
            false
        }
    })
}

/// `fill` returns true on success; entropy source is injected for testing.
fn generate_opaque_with(fill: impl FnOnce(&mut [u8]) -> bool) -> Option<String> {
    use std::fmt::Write;
    let mut buf = [0u8; 16];
    if !fill(&mut buf) {
        return None;
    }
    let mut out = String::with_capacity(OPAQUE_PREFIX.len() + buf.len() * 2);
    out.push_str(OPAQUE_PREFIX);
    for b in buf {
        let _ = write!(out, "{b:02x}");
    }
    Some(out)
}

pub(crate) fn is_opaque(value: &str) -> bool {
    value.starts_with(OPAQUE_PREFIX)
}

wrap_resource_request_handler! {
    pub(super) struct TokenResourceHandler {
        // Per-request slot: the client_id from this exchange's POST body, set in
        // on_before_resource_load and read at response time: the persisted
        // client_id is bound to the same exchange that minted the refresh_token
        // (the SDK binds each token to its context's client_id). Arc, for the
        // macro's per-field Clone shares one slot across both callbacks.
        exchange_client_id: Arc<Mutex<Option<String>>>,
        // Current hop targets the token endpoint; set per on_before_resource_load
        // entry; a redirect hop re-evaluates it, read at response-filter time.
        token_exchange: Arc<AtomicBool>,
        // The session this exchange was issued under, read when the request leaves and
        // carried to the response. This path has no Rust await to capture across (CEF
        // drives it), and the request slot IS the capture point.
        exchange_epoch: Arc<AtomicU64>,
        // Current hop targets the gateway that answers with artwork; set on the same
        // entry that refuses compression for it. Carried rather than re-derived. The
        // response half would otherwise reparse a URL for every response the app makes.
        artwork_host: Arc<AtomicBool>,
    }

    impl ResourceRequestHandler {
        fn on_before_resource_load(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            request: Option<&mut Request>,
            _callback: Option<&mut Callback>,
        ) -> ReturnValue {
            if let Some(req) = request {
                let url_cef = req.url();
                let url = RequestUrl::new(userfree_to_string(&url_cef));
                let token_endpoint = crate::ui::nav::is_token_endpoint(&url);
                self.token_exchange.store(token_endpoint, Ordering::Release);
                if url.is_empty() {
                    return ReturnValue::CONTINUE;
                }

                if token_endpoint {
                    force_identity_encoding(req);
                    capture_client_id(req, &self.exchange_client_id);
                    self.exchange_epoch.store(
                        crate::app_state::with_state(|state| state.session_epoch)
                            .unwrap_or_default(),
                        Ordering::Release,
                    );
                    inject_refresh_token(req, &url);
                }

                // Asked per request rather than per response: the artwork strip
                // reads plaintext, and compression has to be refused while the
                // mime type that gates the strip itself is still unknown.
                //
                // The whole host pays for it, and that breadth is forced rather
                // than lazy. CEF documents the response as unmodifiable in both
                // callbacks that see one, leaving a filter unable to decompress
                // and then say so; plaintext on the wire is the only way to read
                // a body we mean to rewrite. Narrowing by `include` was measured and does
                // not help: 226 of the gateway's 242 read endpoints declare it.
                // Recorded so the next reader does not re-run that search.
                //
                // With the decoder ready, the video artwork is wanted through, and the identity
                // encoding forced below would only cost bandwidth.
                let artwork_host = crate::ui::artwork_filter::arms_artwork_filter(
                    &url,
                    crate::video::codec::is_ready(),
                );
                self.artwork_host.store(artwork_host, Ordering::Release);
                if artwork_host {
                    force_identity_encoding(req);
                    crate::video::codec::note_demand();
                }

                if should_rewrite_token(&url) {
                    rewrite_authorization_header(req);
                }
            }
            ReturnValue::CONTINUE
        }

        fn resource_response_filter(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            request: Option<&mut Request>,
            response: Option<&mut Response>,
        ) -> Option<ResponseFilter> {
            if self.token_exchange.load(Ordering::Acquire) {
                let exchange_cid = self
                    .exchange_client_id
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                let exchange_epoch = self.exchange_epoch.load(Ordering::Acquire);
                return Some(new_buffering_filter(
                    0,
                    Arc::new(move |body| {
                        match process_token_response(
                            &body,
                            exchange_cid.as_deref(),
                            exchange_epoch,
                        ) {
                            ProcessResult::Modified(v) => FilterOutcome::Emit(v),
                            ProcessResult::Passthrough => FilterOutcome::Emit(body),
                            ProcessResult::Error => FilterOutcome::Drop,
                        }
                    }),
                ));
            }

            // One atomic load for every response the app makes; the string below
            // is built only for the handful this gateway answers.
            if !self.artwork_host.load(Ordering::Acquire) {
                return None;
            }
            let mime = response
                .as_ref()
                .map(|r| userfree_to_string(&r.mime_type()))
                .unwrap_or_default();
            if !crate::ui::artwork_filter::carries_json(&mime) {
                return None;
            }
            // An album document is kilobytes; a paged collection carrying its
            // artworks runs larger, and the buffer grows from here either way.
            Some(new_buffering_filter(
                64 * 1024,
                Arc::new(|body| {
                    use crate::ui::artwork_filter::StripResult;
                    match crate::ui::artwork_filter::strip_video_artwork(&body) {
                        StripResult::Rewritten(stripped) => FilterOutcome::Emit(stripped),
                        StripResult::Unchanged => FilterOutcome::Emit(body),
                    }
                }),
            ))
        }
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/ui/token_filter.rs"]
mod tests;
