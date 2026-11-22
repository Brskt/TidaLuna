use cef::*;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::app_state::{GrantKind, UserCommit};
use crate::ui::buffering_filter::{FilterOutcome, force_identity_encoding, new_buffering_filter};
use crate::ui::hosts::{HostShape, TidalDomain};
use crate::ui::nav::{Asker, RequestUrl};

mod request;
mod response;

pub(crate) use request::BodyTokenSurface;
#[cfg(test)]
pub(crate) use request::resolve_opaque_access;
use request::{
    PreparedToken, TokenSender, prepare_token_request, read_token_body, settle_authorization_header,
};
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

pub(crate) fn should_rewrite_token(url: &RequestUrl) -> bool {
    let Some(parsed) = url.parsed() else {
        return false;
    };
    if parsed.scheme() != "https" {
        return false;
    }
    let host = parsed.host_str().unwrap_or("");
    crate::ui::nav::is_tidal_api_host(host)
        || host == crate::ui::nav::HOST_LOGIN
        || host == crate::ui::nav::HOST_AUTH
        || (host == "fp.fa.tidal.com" && parsed.path().starts_with("/license"))
        || is_event_collector(url)
}

/// TIDAL's telemetry hosts, `event-collector.<region>.tidalhi.fi`; the region-less
/// `event-collector.tidalhi.fi` passes too, as it always has.
fn is_event_collector(url: &RequestUrl) -> bool {
    matches!(url.shape(), Some(HostShape::Name(name))
        if name.domain() == Some(TidalDomain::TidalHi) && name.first_label() == "event-collector")
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

/// The random bytes of a nonce, written as lowercase hex after the prefix. The generator and the
/// detector both read it; a nonce the one makes is always one the other finds.
const OPAQUE_BYTES: usize = 16;

/// `fill` returns true on success; entropy source is injected for testing.
fn generate_opaque_with(fill: impl FnOnce(&mut [u8]) -> bool) -> Option<String> {
    use std::fmt::Write;
    let mut buf = [0u8; OPAQUE_BYTES];
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

/// Whether `text` holds a nonce's exact shape anywhere: the prefix and the hex of its bytes. A
/// bare `luna_` inside a real token or a PKCE verifier is not one and is never refused for life.
pub(crate) fn carries_nonce_shape(text: &str) -> bool {
    let hex_len = OPAQUE_BYTES * 2;
    text.match_indices(OPAQUE_PREFIX).any(|(at, _)| {
        text.as_bytes()
            .get(at + OPAQUE_PREFIX.len()..at + OPAQUE_PREFIX.len() + hex_len)
            .is_some_and(|hex| hex.iter().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
    })
}

/// What a request asked the token endpoint, as Rust sent it, read when the answer arrives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExchangeOrigin {
    /// A refresh whose nonce Rust resolved.
    Substituted,
    /// Any other grant the page sends: a sign-in's code exchange.
    Code,
    /// The app-level `client_credentials` grant.
    ClientCredentials,
    /// The SDK's web-session exchange, forwarded only while Rust holds a session.
    WebSession,
    /// The answer must never reach the page holding a token: a refresh Rust did not substitute
    /// (sent empty, or carrying a real token), or a redirect that left the token endpoint or
    /// changed what was asked.
    Inert,
}

/// Where an answer is committed. Its kind is admitted only when the request could produce it: a
/// forged query cannot turn a refresh's answer into an app token, nor a refresh into a sign-in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Answer {
    Drop,
    User(UserCommit),
    Application,
    /// The current generation's own user, taken in place under its nonces; never a new session.
    SameUserInPlace,
}

impl ExchangeOrigin {
    /// A substituted refresh that rotated its refresh token is still a refresh: same user only.
    pub(crate) fn answer(self, kind: GrantKind) -> Answer {
        match (self, kind) {
            (Self::Substituted, GrantKind::Login | GrantKind::Refresh) => {
                Answer::User(UserCommit::Refresh)
            }
            (Self::Code, GrantKind::Login) => Answer::User(UserCommit::Login),
            (Self::ClientCredentials, GrantKind::Application) => Answer::Application,
            (Self::WebSession, GrantKind::Login) => Answer::SameUserInPlace,
            _ => Answer::Drop,
        }
    }

    /// Sticky across hops: a redirect keeps the origin only by asking the same thing again, and
    /// anything else can only lower it.
    pub(crate) fn carry(prev: Option<Self>, hop: Option<Self>) -> Option<Self> {
        match (prev, hop) {
            (None, hop) => hop,
            (Some(prev), Some(hop)) if prev == hop => Some(prev),
            _ => Some(Self::Inert),
        }
    }

    fn load(slot: &AtomicU8) -> Option<Self> {
        match slot.load(Ordering::Acquire) {
            1 => Some(Self::Substituted),
            2 => Some(Self::Code),
            3 => Some(Self::ClientCredentials),
            4 => Some(Self::Inert),
            5 => Some(Self::WebSession),
            _ => None,
        }
    }

    fn store(slot: &AtomicU8, origin: Option<Self>) {
        let raw = match origin {
            None => 0,
            Some(Self::Substituted) => 1,
            Some(Self::Code) => 2,
            Some(Self::ClientCredentials) => 3,
            Some(Self::Inert) => 4,
            Some(Self::WebSession) => 5,
        };
        slot.store(raw, Ordering::Release);
    }
}

/// Cancels a request whose token could only leave unresolved. One line, the reason only: never
/// the body, the header or the nonce.
fn withhold(why: request::TokenRefusal) -> ReturnValue {
    crate::verr!("[AUTH] request withheld: {why}");
    ReturnValue::CANCEL
}

/// The token filter of one hop, its slots fresh: CEF asks for a handler again at every redirect.
pub(super) fn token_handler(asker: Asker) -> ResourceRequestHandler {
    TokenResourceHandler::new(
        Arc::new(Mutex::new(None)),
        Arc::new(AtomicU8::new(0)),
        Arc::new(AtomicU64::new(0)),
        Arc::new(Mutex::new(None)),
        asker,
        Arc::new(AtomicBool::new(false)),
    )
}

wrap_resource_request_handler! {
    pub(super) struct TokenResourceHandler {
        // The client_id this exchange's POST body was SENT with: set in on_before_resource_load,
        // read at response time, because the persisted client_id belongs to the exchange that
        // minted the refresh_token. An Arc: the macro clones each field, both callbacks share it.
        exchange_client_id: Arc<Mutex<Option<String>>>,
        // `ExchangeOrigin` of the request, 0 while no hop reached the token endpoint; each hop
        // folds into it through `carry`, and the response filter is armed when it is set.
        exchange_origin: Arc<AtomicU8>,
        // The session this exchange was issued under, read when the request leaves and
        // carried to the response. This path has no Rust await to capture across (CEF
        // drives it), and the request slot IS the capture point.
        exchange_epoch: Arc<AtomicU64>,
        // The sign-in return a code exchange was admitted on, spent by the commit of its answer.
        exchange_code: Arc<Mutex<Option<crate::ui::nav::LoginProof>>>,
        // Who asked, read on the request's initiator by the client's handler.
        asker: Asker,
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
                let method = userfree_to_string(&req.method());
                if let Some(surface) = BodyTokenSurface::of(&method, &url) {
                    return withhold(surface.refusal());
                }
                let mut hop = None;
                if crate::ui::nav::is_token_endpoint(&url) {
                    force_identity_encoding(req);
                    let body = read_token_body(req);
                    let sender = TokenSender::of(self.asker);
                    let prepared = match prepare_token_request(req, &url, &body, sender) {
                        PreparedToken::Send(prepared) => prepared,
                        PreparedToken::Withhold(why) => return withhold(why),
                    };
                    *self
                        .exchange_client_id
                        .lock()
                        .unwrap_or_else(|e| e.into_inner()) = prepared.client_id;
                    self.exchange_epoch
                        .store(prepared.session_epoch, Ordering::Release);
                    *self.exchange_code.lock().unwrap_or_else(|e| e.into_inner()) = prepared.code;
                    hop = Some(prepared.origin);
                }
                let origin = ExchangeOrigin::carry(ExchangeOrigin::load(&self.exchange_origin), hop);
                ExchangeOrigin::store(&self.exchange_origin, origin);
                if url.is_empty() {
                    return ReturnValue::CONTINUE;
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

                if should_rewrite_token(&url) && !settle_authorization_header(req) {
                    crate::verr!("[AUTH] request withheld: an unresolved bearer could not be removed");
                    return ReturnValue::CANCEL;
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
            if let Some(origin) = ExchangeOrigin::load(&self.exchange_origin) {
                let exchange_cid = self
                    .exchange_client_id
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                let exchange_epoch = self.exchange_epoch.load(Ordering::Acquire);
                let exchange_code = *self.exchange_code.lock().unwrap_or_else(|e| e.into_inner());
                return Some(new_buffering_filter(
                    0,
                    Arc::new(move |body| {
                        match process_token_response(
                            &body,
                            origin,
                            exchange_cid.as_deref(),
                            exchange_epoch,
                            exchange_code,
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
