use std::sync::Arc;
use std::sync::atomic::Ordering;

use cef::*;

use crate::ui::token_filter::userfree_to_string;

use super::frames::{FrameBody, open_frames};
use super::store::{read_capped_body, store_answer};
use super::video::video_answer;
use super::{
    Answer, COVER_SCHEME, CoverRoute, CoverShared, FRAME_STREAMS, SkipVerdict, parse_request,
    requester_refusal,
};

/// Hand one request its answer and resume CEF on its IO thread, where `cancel()` also runs: the
/// `cancelled` check before `cont()` cannot then race a cancel.
fn deliver(shared: Arc<CoverShared>, callback: Callback, answer: Answer) {
    *shared.answer.lock().unwrap_or_else(|e| e.into_inner()) = answer;
    let mut task = CoverContinueTask::new(callback, shared);
    post_task(ThreadId::IO, Some(&mut task));
}

wrap_resource_handler! {
    pub(super) struct CoverHandler {
        shared: Arc<CoverShared>,
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
            // Async path: return now and finish via `callback` once the work resolves.
            if let Some(hr) = handle_request {
                *hr = 0;
            }
            let Some(callback) = callback else {
                return 0;
            };
            let callback = callback.clone();
            let shared = self.shared.clone();
            let Some(request) = request else {
                deliver(shared, callback, Answer::refusal(400, "the cover request is missing"));
                return 1;
            };

            // Before anything that costs: no work starts for a refused requester.
            if let Some(reason) = self.refusal {
                deliver(shared, callback, Answer::refusal(403, reason));
                return 1;
            }
            let (route, key) = match parse_request(&userfree_to_string(&request.url())) {
                Ok(parsed) => parsed,
                Err(why) => {
                    deliver(shared, callback, Answer::refusal(400, why));
                    return 1;
                }
            };
            if userfree_to_string(&request.method()) != route.method() {
                deliver(shared, callback, Answer::refusal(405, "the cover route takes another method"));
                return 1;
            }

            match route {
                CoverRoute::Video => {
                    let range = userfree_to_string(
                        &request.header_by_name(Some(&CefString::from("Range"))),
                    );
                    crate::state::rt_handle().spawn(async move {
                        // The codec gates the cache too: a cover must not play with the switch off.
                        // Asked rather than read: at launch the installed codec is still being
                        // verified, and a cached cover refused then would be transcoded again.
                        if let Err(why) = crate::video::codec::decoder().await {
                            deliver(shared, callback, Answer::refusal(503, why.phrase()));
                            return;
                        }
                        let range = (!range.is_empty()).then_some(range);
                        let answer = tokio::task::spawn_blocking(move || {
                            video_answer(&key, range.as_deref())
                        })
                        .await
                        .unwrap_or_else(|_| Answer::refusal(500, "the cover could not be read"));
                        deliver(shared, callback, answer);
                    });
                }
                CoverRoute::Frames => {
                    let Some(slot) = FRAME_STREAMS.try_acquire() else {
                        deliver(shared, callback, Answer::refusal(429, "too many covers are being transcoded"));
                        return 1;
                    };
                    crate::state::rt_handle().spawn(async move {
                        let answer = match open_frames(&shared, &key).await {
                            Ok(stream) => {
                                *shared.frames.lock().unwrap_or_else(|e| e.into_inner()) =
                                    Some(FrameBody::new(stream, key, slot));
                                shared.streaming.store(true, Ordering::Release);
                                Answer {
                                    status: 200,
                                    mime: "application/octet-stream",
                                    ..Answer::default()
                                }
                            }
                            Err(answer) => answer,
                        };
                        deliver(shared, callback, answer);
                    });
                }
                CoverRoute::Store => {
                    let body = match read_capped_body(request, crate::video::webm::MAX_WEBM_BYTES) {
                        Ok(body) => body,
                        Err((status, why)) => {
                            deliver(shared, callback, Answer::refusal(status, why));
                            return 1;
                        }
                    };
                    crate::state::rt_handle().spawn(async move {
                        let answer =
                            tokio::task::spawn_blocking(move || store_answer(&key, &body))
                                .await
                                .unwrap_or_else(|_| {
                                    Answer::refusal(500, "the cover could not be stored")
                                });
                        deliver(shared, callback, answer);
                    });
                }
            }
            1
        }

        fn response_headers(
            &self,
            response: Option<&mut Response>,
            response_length: Option<&mut i64>,
            _redirect_url: Option<&mut CefString>,
        ) {
            let answer = self.shared.answer.lock().unwrap_or_else(|e| e.into_inner());
            let Some(resp) = response else {
                return;
            };
            let header = |resp: &mut Response, name: &str, value: &str| {
                resp.set_header_by_name(
                    Some(&CefString::from(name)),
                    Some(&CefString::from(value)),
                    1,
                );
            };
            // An answer never set is a fault of ours, never the network the page would retry.
            resp.set_status(if answer.status == 0 { 500 } else { answer.status as i32 });
            let mime = if answer.mime.is_empty() { "text/plain" } else { answer.mime };
            resp.set_mime_type(Some(&CefString::from(mime)));
            header(resp, "Access-Control-Allow-Origin", "*");
            // Exposed as well as allowed: a standard scheme has its own origin, which makes the
            // page's fetch cross-origin, and CORS shows it only safelisted headers. Without this,
            // every refusal would reach the page as a bare status code.
            header(resp, "Access-Control-Expose-Headers", "X-Luna-Cover-Reason");
            // The named reason travels as a header so the page can say why a cover stayed still
            // without a second round trip.
            if !answer.reason.is_empty() {
                header(resp, "X-Luna-Cover-Reason", &answer.reason);
            }
            if answer.accept_ranges {
                header(resp, "Accept-Ranges", "bytes");
            }
            if let Some(range) = &answer.content_range {
                header(resp, "Content-Range", range);
            }
            // A refusal is about this moment (the codec, the cache, the cap), never the cover.
            if answer.status >= 400 {
                header(resp, "Cache-Control", "no-store");
            }
            if let Some(len) = response_length {
                *len = if self.shared.streaming.load(Ordering::Acquire) {
                    // Decoded as it is read: the length is not known up front.
                    -1
                } else {
                    answer.body.len() as i64
                };
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
            // SAFETY: CEF guarantees `data_out` is writable for at least `bytes_to_read` bytes for
            // the duration of this call, and nothing else holds it meanwhile.
            let out = unsafe { std::slice::from_raw_parts_mut(data_out, bytes_to_read as usize) };

            if self.shared.streaming.load(Ordering::Acquire) {
                // Decoded here, on the pool sequence CEF reads on: one frame is little work, and
                // nothing is parked waiting for one, which would hold `cancel()` back.
                let mut frames = self.shared.frames.lock().unwrap_or_else(|e| e.into_inner());
                let Some(body) = frames.as_mut() else {
                    *br = 0;
                    return 0;
                };
                if self.shared.cancelled.load(Ordering::Relaxed) {
                    *frames = None;
                    *br = 0;
                    return 0;
                }
                return match body.fill(out) {
                    Ok(0) => {
                        // Frees the decoder, the file and the stream's slot now, not at the
                        // handler's release.
                        *frames = None;
                        *br = 0;
                        0
                    }
                    Ok(n) => {
                        *br = n as ::std::os::raw::c_int;
                        1
                    }
                    Err(e) => {
                        crate::verr!("[VIDEO]   Cover frames stopped: {e}");
                        *frames = None;
                        // `ERR_FAILED`: the body ends in an error, never in a short success the
                        // page would take for the whole cover.
                        *br = -2;
                        0
                    }
                };
            }

            let mut answer = self.shared.answer.lock().unwrap_or_else(|e| e.into_inner());
            let offset = answer.offset;
            let left = answer.body.len().saturating_sub(offset);
            if left == 0 {
                *br = 0;
                return 0; // EOF
            }
            let n = left.min(out.len());
            out[..n].copy_from_slice(&answer.body[offset..offset + n]);
            answer.offset += n;
            *br = n as ::std::os::raw::c_int;
            1
        }

        fn skip(
            &self,
            bytes_to_skip: i64,
            bytes_skipped: Option<&mut i64>,
            _callback: Option<&mut ResourceSkipCallback>,
        ) -> ::std::os::raw::c_int {
            let Some(skipped) = bytes_skipped else {
                return 0;
            };
            let verdict = self
                .shared
                .answer
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .skip(bytes_to_skip);
            match verdict {
                SkipVerdict::Skipped(n) => {
                    *skipped = n;
                    1
                }
                // `ERR_FAILED`, as CEF's own default: a refused range is an error, never a
                // body placed at the wrong offset.
                SkipVerdict::Refused => {
                    *skipped = -2;
                    0
                }
            }
        }

        fn cancel(&self) {
            // Only the flag: the download stops at its next chunk, a decode at its next `read`,
            // and the stream and its slot go with the handler.
            self.shared.cancelled.store(true, Ordering::Relaxed);
        }
    }
}

wrap_task! {
    struct CoverContinueTask {
        callback: Callback,
        shared: Arc<CoverShared>,
    }
    impl Task {
        fn execute(&self) {
            if self.shared.cancelled.load(Ordering::Relaxed) {
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
            Some(CoverHandler::new(Arc::new(CoverShared::default()), refusal))
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
