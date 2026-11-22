use cef::*;
use std::sync::Mutex;

use super::{carries_nonce_shape, is_opaque, userfree_to_string};
use crate::app_state::AppToken;
use crate::platform::secure_store::{StoredTokenState, TokenGeneration};
use crate::ui::hosts::HostShape;
use crate::ui::nav::RequestUrl;
use crate::util::now_unix_secs;

const AUTHORIZATION: &str = "Authorization";
const PATH_SESSION_HYDRATE: &str = "/api/session/hydrate";

fn read_post_body(req: &mut Request) -> Option<Vec<u8>> {
    let method_cef = req.method();
    let method = userfree_to_string(&method_cef);
    if method != "POST" {
        return None;
    }
    let post_data = req.post_data()?;
    let count = post_data.element_count();
    if count == 0 {
        return None;
    }
    let mut elements: Vec<Option<PostDataElement>> = vec![None; count];
    post_data.elements(Some(&mut elements));
    let total: usize = elements.iter().flatten().map(|el| el.bytes_count()).sum();
    if total == 0 {
        return None;
    }
    let mut body = Vec::with_capacity(total);
    for el in elements.into_iter().flatten() {
        let n = el.bytes_count();
        if n == 0 {
            continue;
        }
        let mut buf = vec![0u8; n];
        el.bytes(n, buf.as_mut_ptr());
        body.extend_from_slice(&buf);
    }
    if body.is_empty() { None } else { Some(body) }
}

/// The first non-empty `client_id` of a form-encoded token request body.
pub(crate) fn form_client_id(body: &str) -> Option<String> {
    url::form_urlencoded::parse(body.as_bytes())
        .find(|(k, v)| k == "client_id" && !v.is_empty())
        .map(|(_, v)| v.into_owned())
}

pub(super) fn capture_client_id(req: &mut Request, exchange_slot: &Mutex<Option<String>>) {
    let Some(body_bytes) = read_post_body(req) else {
        return;
    };
    let Some(cid) = std::str::from_utf8(&body_bytes)
        .ok()
        .and_then(form_client_id)
    else {
        return;
    };
    // Bind this exchange's client_id to its own response (per-request slot).
    *exchange_slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(cid);
}

/// The generation an opaque nonce names: `previous` while its window is open and the nonce is
/// one of its own, `current` for ANY other `luna_*`. A nonce only ever means "the current user's
/// token"; a raw opaque left on the wire would be rejected by TIDAL and leak the placeholder.
fn generation_named_by(
    ts: &StoredTokenState,
    now: u64,
    names_it: impl FnOnce(&TokenGeneration) -> bool,
) -> &TokenGeneration {
    match ts.previous.as_ref() {
        Some(prev)
            if names_it(prev) && ts.previous_valid_until.is_none_or(|until| now <= until) =>
        {
            prev
        }
        _ => &ts.current,
    }
}

/// The generation whose access token an opaque `luna_*` access nonce stands for on the wire.
/// The caller must have checked `is_opaque`; callers gate this on `token_state` = Some: a stray
/// opaque during a logged-out window resolves to nothing.
pub(crate) fn resolve_opaque_access<'a>(
    ts: &'a StoredTokenState,
    opaque: &str,
    now: u64,
) -> &'a TokenGeneration {
    generation_named_by(ts, now, |g| g.opaque_at == opaque)
}

/// Refresh-token counterpart of [`resolve_opaque_access`]. Keeps the refresh
/// side self-healing: a `luna_*` the SDK persisted past our two-entry window
/// still maps to the current real refresh token instead of leaving raw - which
/// is what let TIDAL reject the SDK's ~1h refresh and log the user out.
pub(crate) fn resolve_opaque_refresh(
    ts: &crate::platform::secure_store::StoredTokenState,
    opaque: &str,
    now: u64,
) -> String {
    if let Some(prev) = ts.previous.as_ref()
        && opaque == prev.opaque_rt
        && ts.previous_valid_until.is_none_or(|until| now <= until)
    {
        return prev.refresh_token.clone();
    }
    ts.current.refresh_token.clone()
}

pub(super) fn inject_refresh_token(req: &mut Request, url: &RequestUrl) {
    if !crate::ui::nav::is_token_endpoint(url) {
        return;
    }
    let Some(body_bytes) = read_post_body(req) else {
        return;
    };
    let Ok(body_str) = std::str::from_utf8(&body_bytes) else {
        return;
    };
    let params: Vec<(String, String)> = url::form_urlencoded::parse(body_str.as_bytes())
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();

    let is_refresh = params
        .iter()
        .any(|(k, v)| k == "grant_type" && v == "refresh_token");
    if !is_refresh {
        return;
    }

    let Some(rt_value) = params
        .iter()
        .find(|(k, _)| k == "refresh_token")
        .map(|(_, v)| v.as_str())
    else {
        return;
    };

    if !is_opaque(rt_value) {
        return;
    }

    // None only when logged out (no token_state); otherwise every luna_*
    // resolves to the current real refresh token; the SDK's own refresh
    // never leaves with a raw opaque (the ~1h logout).
    let real_rt = crate::app_state::with_state(|state| {
        state
            .token_state
            .as_ref()
            .map(|ts| resolve_opaque_refresh(ts, rt_value, now_unix_secs()))
    })
    .flatten();

    let Some(real_rt) = real_rt else { return };

    let new_body: String = params
        .iter()
        .map(|(k, v)| {
            if k == "refresh_token" {
                format!(
                    "{}={}",
                    url::form_urlencoded::byte_serialize(k.as_bytes()).collect::<String>(),
                    url::form_urlencoded::byte_serialize(real_rt.as_bytes()).collect::<String>()
                )
            } else {
                format!(
                    "{}={}",
                    url::form_urlencoded::byte_serialize(k.as_bytes()).collect::<String>(),
                    url::form_urlencoded::byte_serialize(v.as_bytes()).collect::<String>()
                )
            }
        })
        .collect::<Vec<_>>()
        .join("&");

    if let Some(mut new_pd) = post_data_create()
        && let Some(mut el) = post_data_element_create()
    {
        let bytes = new_body.as_bytes();
        el.set_to_bytes(bytes.len(), bytes.as_ptr());
        new_pd.add_element(Some(&mut el));
        req.set_post_data(Some(&mut new_pd));
    }
}

/// What leaves in the `Authorization` header of a request to a TIDAL API host.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum BearerFate {
    /// The full header value to send: the nonce resolved to its generation's access token.
    Substitute(String),
    /// A nonce that resolves to nothing: the header goes, rather than the placeholder.
    Strip,
    Leave,
}

impl BearerFate {
    /// The scheme matches without case and the value is read trimmed; any header still carrying a
    /// nonce's shape that this does not resolve is stripped.
    pub(crate) fn decide(header: &str, held: &HeldTokens, now: u64) -> Self {
        let Some(opaque) = bearer_value(header).filter(|v| is_opaque(v)) else {
            return if carries_nonce_shape(header) {
                Self::Strip
            } else {
                Self::Leave
            };
        };
        let token = match NonceTarget::of(held, opaque, now) {
            NonceTarget::User(generation) => &generation.access_token,
            NonceTarget::Application(app) => &app.access_token,
            NonceTarget::Unresolvable => return Self::Strip,
        };
        Self::Substitute(format!("Bearer {token}"))
    }
}

pub(crate) struct HeldTokens<'a> {
    pub(crate) user: Option<&'a StoredTokenState>,
    pub(crate) app: Option<&'a AppToken>,
    /// A user session the page still runs under: an unknown nonce then means its current token.
    pub(crate) user_live: bool,
}

pub(crate) enum NonceTarget<'a> {
    User(&'a TokenGeneration),
    Application(&'a AppToken),
    /// Nothing held: the header goes, never the placeholder and never a request cancelled for it.
    Unresolvable,
}

impl<'a> NonceTarget<'a> {
    /// A nonce one of the user's generations minted resolves to it, and the app token's to the app
    /// token. An unknown one means the live user's current token, else the app token, which the
    /// page also holds without a user.
    pub(crate) fn of(held: &HeldTokens<'a>, nonce: &str, now: u64) -> Self {
        let named = held
            .user
            .map(|ts| resolve_opaque_access(ts, nonce, now))
            .filter(|generation| generation.opaque_at == nonce);
        if let Some(generation) = named {
            return Self::User(generation);
        }
        if let Some(app) = held.app.filter(|app| app.opaque_at == nonce) {
            return Self::Application(app);
        }
        match (held.user.filter(|_| held.user_live), held.app) {
            (Some(ts), _) => Self::User(&ts.current),
            (None, Some(app)) => Self::Application(app),
            (None, None) => Self::Unresolvable,
        }
    }
}

fn bearer_value(header: &str) -> Option<&str> {
    let ows = |c: char| c == ' ' || c == '\t';
    let (scheme, value) = header.trim_matches(ows).split_once(ows)?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| value.trim_matches(ows))
}

/// Settles the `Authorization` header of a request about to leave. `false` when a header that had
/// to go could not be removed: the caller cancels the request rather than send the nonce.
pub(super) fn settle_authorization_header(req: &mut Request) -> bool {
    let name = CefString::from(AUTHORIZATION);
    let header = userfree_to_string(&req.header_by_name(Some(&name)));
    if header.is_empty() {
        return true;
    }
    let now = now_unix_secs();
    let fate = crate::app_state::with_state(|state| {
        let held = HeldTokens {
            user: state.token_state.as_ref(),
            app: state.app_token.as_ref(),
            user_live: state.token_state.is_some(),
        };
        BearerFate::decide(&header, &held, now)
    })
    .unwrap_or_else(|| {
        let nothing = HeldTokens {
            user: None,
            app: None,
            user_live: false,
        };
        BearerFate::decide(&header, &nothing, now)
    });
    match fate {
        BearerFate::Leave => true,
        BearerFate::Substitute(value) => {
            req.set_header_by_name(Some(&name), Some(&CefString::from(value.as_str())), 1);
            true
        }
        BearerFate::Strip => strip_header(req, AUTHORIZATION),
    }
}

/// Rewrites the whole header map without `name`, compared without case, then reads it back: a
/// header still there after the rewrite is a failure, never assumed gone.
fn strip_header(req: &mut Request, name: &str) -> bool {
    let mut headers = CefStringMultimap::new();
    req.header_map(Some(&mut headers));
    let mut kept = CefStringMultimap::new();
    for (key, values) in headers {
        if key.eq_ignore_ascii_case(name) {
            continue;
        }
        for value in values {
            if !kept.append(&key, &value) {
                return false;
            }
        }
    }
    req.set_header_map(Some(&mut kept));
    userfree_to_string(&req.header_by_name(Some(&CefString::from(name)))).is_empty()
}

/// A page request whose body carries a token for a surface that has no use for it: held back
/// whatever it carries, never resolved (the real token would go to tidal.com for a cookie the
/// system browser never reads).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BodyTokenSurface {
    SessionHydrate,
}

impl BodyTokenSurface {
    pub(crate) fn of(method: &str, url: &RequestUrl) -> Option<Self> {
        let parsed = url.parsed()?;
        let on_apex = matches!(url.shape(), Some(HostShape::Name(name))
            if name.is(crate::ui::hosts::TidalDomain::Tidal.apex()));
        (method == "POST"
            && parsed.scheme() == "https"
            && on_apex
            && parsed.path() == PATH_SESSION_HYDRATE)
            .then_some(Self::SessionHydrate)
    }

    pub(crate) fn refusal(self) -> TokenRefusal {
        match self {
            Self::SessionHydrate => TokenRefusal::SessionHydrate,
        }
    }
}

/// Why a request to a token surface is cancelled instead of sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TokenRefusal {
    SessionHydrate,
}

impl std::fmt::Display for TokenRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::SessionHydrate => "session/hydrate would carry a nonce to tidal.com",
        })
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/ui/token_filter/request.rs"]
mod tests;
