use cef::*;
use std::cell::RefCell;

use crate::ui::token_filter::userfree_to_string;

// TIDAL delivers its CSP as a <meta http-equiv> tag, not a header. Renaming the
// attribute makes Chromium stop enforcing it, unblocking plugin font/image loads.
const CSP_NEEDLE: &[u8] = b"<meta http-equiv=\"Content-Security-Policy\"";
const CSP_REPLACEMENT: &[u8] = b"<meta name=\"LunaWuzHere\"";

#[derive(Clone)]
enum FilterState {
    Accumulating(Vec<u8>),
    Emitting { data: Vec<u8>, offset: usize },
    Done,
}

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
            _is_navigation: ::std::os::raw::c_int,
            _is_download: ::std::os::raw::c_int,
            _request_initiator: Option<&CefString>,
            _disable_default_handling: Option<&mut ::std::os::raw::c_int>,
        ) -> Option<ResourceRequestHandler> {
            // The SW precache fetch is browser-less (is_navigation=0) and reaches
            // only the context handler. Scope to HTML docs so assets keep gzip.
            let url = request
                .as_ref()
                .map(|r| userfree_to_string(&r.url()))
                .unwrap_or_default();
            if !is_document_url(&url) {
                return None;
            }
            Some(DocumentHandler::new())
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
            Some(CspStripFilter::new(RefCell::new(
                FilterState::Accumulating(Vec::with_capacity(32 * 1024)),
            )))
        }
    }
}

wrap_response_filter! {
    pub(super) struct CspStripFilter {
        state: RefCell<FilterState>,
    }

    impl ResponseFilter {
        fn init_filter(&self) -> ::std::os::raw::c_int {
            1
        }

        fn filter(
            &self,
            data_in: Option<&mut Vec<u8>>,
            data_in_read: Option<&mut usize>,
            data_out: Option<&mut Vec<u8>>,
            data_out_written: Option<&mut usize>,
        ) -> ResponseFilterStatus {
            let mut state = self.state.borrow_mut();
            let out_written = match data_out_written {
                Some(w) => w,
                None => return ResponseFilterStatus::ERROR,
            };
            *out_written = 0;

            match &mut *state {
                FilterState::Accumulating(buf) => {
                    if let Some(input) = data_in {
                        if let Some(read) = data_in_read {
                            *read = input.len();
                        }
                        buf.extend_from_slice(input);
                        ResponseFilterStatus::NEED_MORE_DATA
                    } else {
                        let accumulated = std::mem::take(buf);
                        let modified = strip_csp_meta(&accumulated);
                        *state = FilterState::Emitting {
                            data: modified,
                            offset: 0,
                        };
                        drop(state);
                        self.emit(data_out, out_written)
                    }
                }
                FilterState::Emitting { .. } => {
                    if let Some(input) = data_in
                        && let Some(read) = data_in_read
                    {
                        *read = input.len();
                    }
                    drop(state);
                    self.emit(data_out, out_written)
                }
                FilterState::Done => ResponseFilterStatus::DONE,
            }
        }
    }
}

impl CspStripFilter {
    fn emit(
        &self,
        data_out: Option<&mut Vec<u8>>,
        out_written: &mut usize,
    ) -> ResponseFilterStatus {
        let mut state = self.state.borrow_mut();
        let (data, offset) = match &mut *state {
            FilterState::Emitting { data, offset } => (data, offset),
            _ => return ResponseFilterStatus::ERROR,
        };

        let remaining = &data[*offset..];
        if remaining.is_empty() {
            *state = FilterState::Done;
            return ResponseFilterStatus::DONE;
        }

        let Some(out_buf) = data_out else {
            return ResponseFilterStatus::NEED_MORE_DATA;
        };
        let to_write = remaining.len().min(out_buf.len());
        out_buf[..to_write].copy_from_slice(&remaining[..to_write]);
        *out_written = to_write;
        *offset += to_write;

        if *offset >= data.len() {
            *state = FilterState::Done;
            ResponseFilterStatus::DONE
        } else {
            ResponseFilterStatus::NEED_MORE_DATA
        }
    }
}

// Only the shell HTML (root nav or *.html on the app host) is stripped; assets
// stay untouched so they keep compression.
pub(crate) fn is_document_url(url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url) else {
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
        assert!(is_document_url("https://desktop.tidal.com/"));
        assert!(is_document_url("https://desktop.tidal.com/index.html"));
        assert!(is_document_url(
            "https://desktop.tidal.com/lastfmcallback.html"
        ));
        assert!(!is_document_url(
            "https://desktop.tidal.com/assets/index-abc.js"
        ));
        assert!(!is_document_url("https://desktop.tidal.com/assets/x.css"));
        assert!(!is_document_url(
            "https://resources.tidal.com/images/x/80x80.jpg"
        ));
        assert!(!is_document_url("https://api.tidal.com/v1/tracks/1"));
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
