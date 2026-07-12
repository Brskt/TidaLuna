use cef::*;
use std::sync::Arc;

use crate::ui::buffering_filter::{FilterOutcome, new_buffering_filter};
use crate::ui::nav::RequestUrl;
use crate::ui::token_filter::userfree_to_string;

// TIDAL delivers its CSP as a <meta http-equiv> tag, not a header. Renaming the
// attribute makes Chromium stop enforcing it, unblocking plugin font/image loads.
const CSP_NEEDLE: &[u8] = b"<meta http-equiv=\"Content-Security-Policy\"";
const CSP_REPLACEMENT: &[u8] = b"<meta name=\"LunaWuzHere\"";

// Catches the browser-less service-worker precache fetch of the shell, which the
// browser-level handler never sees. Browser-associated doc loads are handled there.
wrap_request_context_handler! {
    pub(crate) struct DocumentContextHandler;

    impl RequestContextHandler {
        fn resource_request_handler(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            request: Option<&mut Request>,
            is_navigation: ::std::os::raw::c_int,
            is_download: ::std::os::raw::c_int,
            _request_initiator: Option<&CefString>,
            _disable_default_handling: Option<&mut ::std::os::raw::c_int>,
        ) -> Option<ResourceRequestHandler> {
            // The SW precache fetch is browser-less (is_navigation=0) and reaches
            // only the context handler. Scope to HTML docs so assets keep gzip.
            let url = RequestUrl::new(
                request
                    .as_ref()
                    .map(|r| userfree_to_string(&r.url()))
                    .unwrap_or_default(),
            );
            if is_document_url(&url) {
                return Some(DocumentHandler::new());
            }
            // SW precache of a React-family chunk: rewrite it (browser-less) so
            // the cached chunk carries the capture call on warm loads too.
            if crate::ui::module_capture::target_module_id(&url).is_some() {
                return Some(crate::ui::module_capture::CaptureRequestHandler::new());
            }
            // Store fetches routed through the service worker reach only this context
            // handler; serve `store.json` via reqwest (Chromium rejects the CDN redirect).
            if let Some(h) =
                crate::ui::store_proxy::intercept(url.as_str(), is_navigation, is_download)
            {
                return Some(h);
            }
            None
        }
    }
}

wrap_resource_request_handler! {
    pub(crate) struct DocumentHandler;

    impl ResourceRequestHandler {
        fn on_before_resource_load(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            request: Option<&mut Request>,
            _callback: Option<&mut Callback>,
        ) -> ReturnValue {
            // The filter sees pre-decompression bytes; force identity so the
            // plaintext <meta> tag matches (cf. token_filter).
            if let Some(req) = request {
                let accept_name = CefString::from("Accept-Encoding");
                let accept_val = CefString::from("identity");
                req.set_header_by_name(Some(&accept_name), Some(&accept_val), 1);
            }
            ReturnValue::CONTINUE
        }

        fn resource_response_filter(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _request: Option<&mut Request>,
            response: Option<&mut Response>,
        ) -> Option<ResponseFilter> {
            let mime = response
                .as_ref()
                .map(|r| {
                    let m = r.mime_type();
                    userfree_to_string(&m)
                })
                .unwrap_or_default();
            if !mime.starts_with("text/html") {
                return None;
            }
            Some(new_buffering_filter(
                32 * 1024,
                Arc::new(|body| FilterOutcome::Emit(strip_csp_meta(&body))),
            ))
        }
    }
}

// Only the shell HTML (root nav or *.html on the app host) is stripped; assets
// stay untouched so they keep compression.
pub(crate) fn is_document_url(url: &RequestUrl) -> bool {
    let Some(parsed) = url.parsed() else {
        return false;
    };
    if parsed.host_str() != Some(crate::ui::nav::HOST_DESKTOP) {
        return false;
    }
    let path = parsed.path();
    path == "/" || path.ends_with(".html")
}

fn strip_csp_meta(body: &[u8]) -> Vec<u8> {
    let Some(pos) = body.windows(CSP_NEEDLE.len()).position(|w| w == CSP_NEEDLE) else {
        return body.to_vec();
    };
    let mut out = Vec::with_capacity(body.len() - CSP_NEEDLE.len() + CSP_REPLACEMENT.len());
    out.extend_from_slice(&body[..pos]);
    out.extend_from_slice(CSP_REPLACEMENT);
    out.extend_from_slice(&body[pos + CSP_NEEDLE.len()..]);
    out
}

#[cfg(test)]
mod tests {
    use super::{is_document_url, strip_csp_meta};

    #[test]
    fn document_url_matches_shell_only() {
        let u = |s: &str| crate::ui::nav::RequestUrl::new(s.to_string());
        assert!(is_document_url(&u("https://desktop.tidal.com/")));
        assert!(is_document_url(&u("https://desktop.tidal.com/index.html")));
        assert!(is_document_url(&u(
            "https://desktop.tidal.com/lastfmcallback.html"
        )));
        assert!(!is_document_url(&u(
            "https://desktop.tidal.com/assets/index-abc.js"
        )));
        assert!(!is_document_url(&u(
            "https://desktop.tidal.com/assets/x.css"
        )));
        assert!(!is_document_url(&u(
            "https://resources.tidal.com/images/x/80x80.jpg"
        )));
        assert!(!is_document_url(&u("https://api.tidal.com/v1/tracks/1")));
    }

    #[test]
    fn strips_csp_meta_tag() {
        let html =
            b"<html><head><meta http-equiv=\"Content-Security-Policy\" content=\"x\"></head></html>";
        let out = strip_csp_meta(html);
        let s = std::str::from_utf8(&out).unwrap();
        assert!(s.contains("<meta name=\"LunaWuzHere\""));
        assert!(!s.contains("Content-Security-Policy"));
    }

    #[test]
    fn passthrough_when_absent() {
        let html = b"<html><head></head></html>";
        assert_eq!(strip_csp_meta(html), html);
    }

    #[test]
    fn only_replaces_first() {
        let html = b"<meta http-equiv=\"Content-Security-Policy\" a><meta http-equiv=\"Content-Security-Policy\" b>";
        let out = strip_csp_meta(html);
        let s = std::str::from_utf8(&out).unwrap();
        assert_eq!(s.matches("LunaWuzHere").count(), 1);
        assert_eq!(s.matches("Content-Security-Policy").count(), 1);
    }
}
