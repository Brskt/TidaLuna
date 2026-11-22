//! TIDAL's web-session exchange (`POST auth.tidal.com/v1/web-session/credentials`): the SDK sends
//! it when it holds no token, and TIDAL answers from the cookie with a token bound to the client the
//! JSON body names. Rust forwards it only while it holds a session; otherwise it answers itself.

use cef::*;

use crate::ui::nav::{RequestUrl, is_secure_tidal_target};

/// What becomes of a web-session exchange, decided when CEF asks for its handler.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum F4Plan {
    /// Rust holds a session: the answer is taken in place, for its own user only.
    Forward,
    /// No session: an empty 204 the SDK reads as no web session, allowed to the page that asked.
    Synthetic204 { allow_origin: String },
    /// No session, and no TIDAL origin to allow the answer to.
    Cancel,
}

impl F4Plan {
    /// `allow_origin` is the initiator exactly as CEF serialised it, the string CORS compares.
    pub(crate) fn of(has_session: bool, initiator: Option<&str>) -> Self {
        if has_session {
            return Self::Forward;
        }
        match initiator {
            Some(origin) if is_secure_tidal_target(&RequestUrl::new(origin.to_owned())) => {
                Self::Synthetic204 {
                    allow_origin: origin.to_owned(),
                }
            }
            _ => Self::Cancel,
        }
    }
}

/// How Rust's empty answer ended. A CORS refusal fails it, and the page would then show no
/// internet rather than its sign-in screen: that one is said, the others are not.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SyntheticOutcome {
    Served,
    Failed,
    /// The page dropped the request itself.
    Aborted,
}

impl SyntheticOutcome {
    pub(crate) fn of(status: UrlrequestStatus, error: Errorcode) -> Self {
        if status == UrlrequestStatus::SUCCESS {
            Self::Served
        } else if status == UrlrequestStatus::CANCELED || error == Errorcode::ABORTED {
            Self::Aborted
        } else {
            Self::Failed
        }
    }
}

wrap_resource_request_handler! {
    pub(super) struct NoSessionAnswer {
        allow_origin: String,
    }

    impl ResourceRequestHandler {
        // The trait default cancels the request before `resource_handler` can answer it.
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
            Some(NoContent::new(self.allow_origin.clone()))
        }

        fn on_resource_load_complete(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _request: Option<&mut Request>,
            response: Option<&mut Response>,
            status: UrlrequestStatus,
            _received_content_length: i64,
        ) {
            let error = response.map_or(Errorcode::FAILED, |r| r.error());
            if SyntheticOutcome::of(status, error) == SyntheticOutcome::Failed {
                crate::verr!(
                    "[AUTH] the empty web-session answer failed ({status:?}, {error:?}): the page may show no internet"
                );
            }
        }
    }
}

// A 204 with no body; no response filter reads it, none being armed on this handler.
wrap_resource_handler! {
    struct NoContent {
        allow_origin: String,
    }

    impl ResourceHandler {
        fn open(
            &self,
            _request: Option<&mut Request>,
            handle_request: Option<&mut ::std::os::raw::c_int>,
            _callback: Option<&mut Callback>,
        ) -> ::std::os::raw::c_int {
            if let Some(handle_request) = handle_request {
                *handle_request = 1;
            }
            1
        }

        fn response_headers(
            &self,
            response: Option<&mut Response>,
            response_length: Option<&mut i64>,
            _redirect_url: Option<&mut CefString>,
        ) {
            if let Some(response) = response {
                response.set_status(204);
                response.set_status_text(Some(&CefString::from("No Content")));
                response.set_header_by_name(
                    Some(&CefString::from("Access-Control-Allow-Origin")),
                    Some(&CefString::from(self.allow_origin.as_str())),
                    1,
                );
                response.set_header_by_name(
                    Some(&CefString::from("Access-Control-Allow-Credentials")),
                    Some(&CefString::from("true")),
                    1,
                );
            }
            if let Some(length) = response_length {
                *length = 0;
            }
        }

        fn read(
            &self,
            _data_out: *mut u8,
            _bytes_to_read: ::std::os::raw::c_int,
            bytes_read: Option<&mut ::std::os::raw::c_int>,
            _callback: Option<&mut ResourceReadCallback>,
        ) -> ::std::os::raw::c_int {
            if let Some(bytes_read) = bytes_read {
                *bytes_read = 0;
            }
            0
        }

        fn cancel(&self) {}
    }
}
