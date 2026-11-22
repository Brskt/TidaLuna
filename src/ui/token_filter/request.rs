use cef::*;

use super::{ExchangeOrigin, carries_nonce_shape, is_opaque, userfree_to_string};
use crate::app_state::AppToken;
use crate::platform::secure_store::{StoredTokenState, TokenGeneration};
use crate::ui::hosts::HostShape;
use crate::ui::nav::{Asker, AuthRoute, LoginProof, RequestUrl};
use crate::util::now_unix_secs;

const AUTHORIZATION: &str = "Authorization";
const CONTENT_TYPE: &str = "Content-Type";
const PATH_SESSION_HYDRATE: &str = "/api/session/hydrate";

/// A token-endpoint body as CEF exposes it, judged before a byte of it is read.
pub(crate) enum TokenBody {
    /// No post data, or post data holding no element.
    Absent,
    /// One element of bytes: the form the SDK sends.
    Bytes(Vec<u8>),
    /// A blob, a stream, a file or several elements: what would leave cannot be read here.
    Unreadable,
}

/// The layout half of [`TokenBody`], apart from the CEF objects so a table can pin it.
#[derive(Debug, PartialEq, Eq)]
enum BodyLayout {
    Absent,
    OneBytes,
    Unreadable,
}

fn body_layout(excluded: bool, kinds: &[Option<PostdataelementType>]) -> BodyLayout {
    match kinds {
        _ if excluded => BodyLayout::Unreadable,
        [] => BodyLayout::Absent,
        [Some(kind)] if *kind == PostdataelementType::BYTES => BodyLayout::OneBytes,
        _ => BodyLayout::Unreadable,
    }
}

pub(super) fn read_token_body(req: &mut Request) -> TokenBody {
    let Some(post_data) = req.post_data() else {
        return TokenBody::Absent;
    };
    let count = post_data.element_count();
    let mut elements: Vec<Option<PostDataElement>> = vec![None; count];
    if count > 0 {
        post_data.elements(Some(&mut elements));
    }
    let kinds: Vec<_> = elements
        .iter()
        .map(|el| el.as_ref().map(|el| el.get_type()))
        .collect();
    match body_layout(post_data.has_excluded_elements() != 0, &kinds) {
        BodyLayout::Absent => TokenBody::Absent,
        BodyLayout::Unreadable => TokenBody::Unreadable,
        BodyLayout::OneBytes => {
            let Some(el) = elements.into_iter().flatten().next() else {
                return TokenBody::Unreadable;
            };
            let n = el.bytes_count();
            let mut buf = vec![0u8; n];
            let read = if n == 0 {
                0
            } else {
                el.bytes(n, buf.as_mut_ptr())
            };
            buf.truncate(read);
            TokenBody::Bytes(buf)
        }
    }
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

/// Refresh-token counterpart of [`resolve_opaque_access`]: a `luna_*` the SDK persisted past our
/// two-entry window still maps to the current generation instead of leaving raw. A whole
/// generation, since its refresh token is only good with its own client.
pub(crate) fn resolve_opaque_refresh<'a>(
    ts: &'a StoredTokenState,
    opaque: &str,
    now: u64,
) -> &'a TokenGeneration {
    generation_named_by(ts, now, |g| g.opaque_rt == opaque)
}

/// A refresh as Rust sends it: the body, and the client that body names.
pub(crate) struct RefreshRewrite {
    pub(crate) body: String,
    pub(crate) client_id: Option<String>,
}

/// The `Content-Type` TIDAL's SDK sends its token requests with, set by Rust on every body it sends.
const FORM_CONTENT_TYPE: &str = "application/x-www-form-urlencoded; charset=UTF-8";

/// The keys each grant may send. Anything else is dropped, `scope` among them on a refresh: the
/// server answers with the scope the token was granted.
const REFRESH_KEYS: [&str; 4] = ["client_id", "client_secret", "grant_type", "refresh_token"];
const CODE_KEYS: [&str; 8] = [
    "client_id",
    "client_secret",
    "client_unique_key",
    "code",
    "code_verifier",
    "grant_type",
    "redirect_uri",
    "scope",
];
/// Exactly these, each once: an extra key on the grant that mints the app token is refused.
const CLIENT_CREDENTIALS_KEYS: [&str; 3] = ["client_id", "client_secret", "grant_type"];

/// A token request normalised by Rust: a grant it knows, each allowed key's first occurrence (the
/// one the endpoint reads), in the order the page sent them. The page's bytes never leave: Rust
/// encodes this.
pub(crate) struct GrantRequest {
    grant: RequestedGrant,
    pairs: Vec<(String, String)>,
}

impl GrantRequest {
    pub(crate) fn parse(body: &str) -> Result<Self, TokenRefusal> {
        let all: Vec<(String, String)> = url::form_urlencoded::parse(body.as_bytes())
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        let mut firsts: Vec<(String, String)> = Vec::new();
        for (key, value) in &all {
            if !firsts.iter().any(|(seen, _)| seen == key) {
                firsts.push((key.clone(), value.clone()));
            }
        }
        let grant_type = firsts
            .iter()
            .find(|(k, _)| k == "grant_type")
            .map(|(_, v)| v.as_str());
        let grant = RequestedGrant::of(grant_type);
        let allowed: &[&str] = match grant {
            RequestedGrant::Refresh | RequestedGrant::UpdateClient => &REFRESH_KEYS,
            RequestedGrant::AuthorizationCode => {
                let mut uris = all.iter().filter(|(k, _)| k == "redirect_uri");
                match (uris.next(), uris.next()) {
                    (Some((_, uri)), None) if is_our_redirect(uri) => {}
                    _ => return Err(TokenRefusal::BadRedirect),
                }
                &CODE_KEYS
            }
            RequestedGrant::ClientCredentials => {
                let exact = all.len() == CLIENT_CREDENTIALS_KEYS.len()
                    && CLIENT_CREDENTIALS_KEYS
                        .iter()
                        .all(|key| all.iter().filter(|(k, _)| k == key).count() == 1);
                if !exact {
                    return Err(TokenRefusal::ForeignKeys);
                }
                &CLIENT_CREDENTIALS_KEYS
            }
            RequestedGrant::Other => return Err(TokenRefusal::UnknownGrant),
        };
        firsts.retain(|(key, _)| allowed.contains(&key.as_str()));
        Ok(Self {
            grant,
            pairs: firsts,
        })
    }

    fn first(&self, key: &str) -> Option<&str> {
        self.pairs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// The `client_id` the endpoint reads, its first occurrence, when not empty: a refresh token
    /// minted in reply is bound to it.
    pub(crate) fn client_id(&self) -> Option<&str> {
        self.first("client_id").filter(|c| !c.is_empty())
    }

    pub(crate) fn encode(&self) -> String {
        let mut out = url::form_urlencoded::Serializer::new(String::new());
        for (key, value) in &self.pairs {
            out.append_pair(key, value);
        }
        out.finish()
    }

    /// The refresh with its token sent empty: TIDAL answers it with a 400 the SDK treats as a
    /// sign-out.
    fn emptied(&self) -> String {
        let mut out = url::form_urlencoded::Serializer::new(String::new());
        for (key, value) in &self.pairs {
            out.append_pair(key, if key == "refresh_token" { "" } else { value });
        }
        out.finish()
    }

    /// Rebuilds a refresh the page sent with a `luna_*` refresh token, from the one generation
    /// that nonce names; `None` when no generation holding a refresh token completes it.
    ///
    /// The token is bound to the client that minted it; that generation's client is sent
    /// whatever the page asked, and a secret meant for the page's client stays behind.
    /// `update_client`, the SDK's migration to the page's client, leaves as a plain refresh. Sent
    /// as such, it carried our placeholder and the SDK retried it forever without logging out.
    pub(crate) fn substitute_refresh(
        &self,
        ts: Option<&StoredTokenState>,
        now: u64,
    ) -> Option<RefreshRewrite> {
        self.grant.sdk_exit()?;
        let opaque = self.first("refresh_token").filter(|rt| is_opaque(rt))?;
        let generation = resolve_opaque_refresh(ts?, opaque, now);
        // An app-level generation holds no refresh token: an empty one would be refused all the
        // same.
        if generation.refresh_token.is_empty() {
            return None;
        }

        let asked = self.client_id();
        let update_client = self.grant == RequestedGrant::UpdateClient;
        let sent = generation
            .client
            .for_page_refresh(asked, update_client)
            .map(str::to_owned);
        let client_changed = asked != sent.as_deref();

        let mut out = url::form_urlencoded::Serializer::new(String::new());
        for (key, value) in &self.pairs {
            match key.as_str() {
                "grant_type" => {
                    out.append_pair(key, "refresh_token");
                }
                "refresh_token" => {
                    out.append_pair(key, &generation.refresh_token);
                }
                "client_id" => {
                    if let Some(client) = &sent {
                        out.append_pair(key, client);
                    }
                }
                "client_secret" if client_changed => {}
                _ => {
                    out.append_pair(key, value);
                }
            }
        }
        if self.first("client_id").is_none()
            && let Some(client) = &sent
        {
            out.append_pair("client_id", client);
        }
        Some(RefreshRewrite {
            body: out.finish(),
            client_id: sent,
        })
    }
}

/// The `redirect_uri` of a sign-in: our callback, read exactly as written (the parser drops
/// surrounding whitespace and tabs, which `as_str` then gives away). A query passes: it holds
/// TIDAL's own route parameters (`?album=<id>`).
fn is_our_redirect(value: &str) -> bool {
    let Ok(url) = url::Url::parse(value) else {
        return false;
    };
    url.as_str() == value
        && url.scheme() == "tidal"
        && url.host_str() == Some("login")
        && url.path() == "/auth"
        && url.username().is_empty()
        && url.password().is_none()
        && url.port().is_none()
        && url.fragment().is_none()
}

/// The grant a token request asks for, read on its first `grant_type` as the endpoint reads it.
/// Asked, never believed: what the answer is allowed to commit is not decided here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RequestedGrant {
    Refresh,
    UpdateClient,
    AuthorizationCode,
    ClientCredentials,
    Other,
}

/// What the SDK does when its refresh comes back refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SdkExit {
    /// A refused `refresh_token` signs the page out: an emptied one ends the session cleanly.
    LogsOut,
    /// A refused `update_client` is retried forever: sending it at all only feeds the loop.
    Retries,
}

impl RequestedGrant {
    fn of(grant_type: Option<&str>) -> Self {
        match grant_type {
            Some("refresh_token") => Self::Refresh,
            Some("update_client") => Self::UpdateClient,
            Some("authorization_code") => Self::AuthorizationCode,
            Some("client_credentials") => Self::ClientCredentials,
            _ => Self::Other,
        }
    }

    /// `None` for a grant that never carries one of our refresh nonces.
    pub(crate) fn sdk_exit(self) -> Option<SdkExit> {
        match self {
            Self::Refresh => Some(SdkExit::LogsOut),
            Self::UpdateClient => Some(SdkExit::Retries),
            Self::AuthorizationCode | Self::ClientCredentials | Self::Other => None,
        }
    }

    /// What the answer to a request Rust sent without substituting a nonce may do. A refresh
    /// without our nonce names no generation of ours: its answer never commits.
    fn unsubstituted_origin(self) -> ExchangeOrigin {
        match self {
            Self::Refresh | Self::UpdateClient | Self::Other => ExchangeOrigin::Inert,
            Self::AuthorizationCode => ExchangeOrigin::Code,
            Self::ClientCredentials => ExchangeOrigin::ClientCredentials,
        }
    }
}

/// Why a request to a token surface is cancelled instead of sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TokenRefusal {
    Unreadable,
    NotUtf8,
    ControlByte,
    NonceMisplaced,
    UnknownGrant,
    ForeignKeys,
    BadRedirect,
    NoLoginProof,
    UpdateClientUnresolved,
    BodyNotReplaced,
    SessionHydrate,
    UnexpectedBody,
    NoRoute,
}

impl std::fmt::Display for TokenRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Unreadable => "its body is not one element of bytes",
            Self::NotUtf8 => "its body is not UTF-8",
            Self::ControlByte => "its body holds a control byte",
            Self::UnknownGrant => "it asks for a grant TIDAL's app never sends",
            Self::ForeignKeys => "client_credentials carries keys beyond its three",
            Self::BadRedirect => "its redirect_uri is not our callback, exactly once",
            Self::NoLoginProof => "its code is not the one the sign-in page returned",
            Self::NonceMisplaced => "a nonce sits outside the first refresh_token of a refresh",
            Self::UpdateClientUnresolved => {
                "update_client names no session holding a refresh token"
            }
            Self::BodyNotReplaced => "the rewritten body could not be attached",
            Self::SessionHydrate => "session/hydrate would carry a nonce to tidal.com",
            Self::UnexpectedBody => "it carries a body its route never sends",
            Self::NoRoute => "it names no sign-in route",
        })
    }
}

/// What leaves for the token endpoint in place of the page's request.
pub(crate) enum TokenFate {
    /// A nonce resolved: the refresh leaves as Rust rebuilt it.
    Substitute(RefreshRewrite),
    /// A nonce that resolves to nothing, on a refresh: the token is sent empty, the endpoint
    /// refuses it and the SDK signs out.
    Neutralise(String),
    /// No nonce: the request leaves as Rust re-encoded it, its answer limited by its grant. A code
    /// exchange names the code it carries.
    Reissue {
        body: String,
        client_id: Option<String>,
        origin: ExchangeOrigin,
        code: Option<LoginProof>,
    },
    /// The sign-in page's own request leaves as it came, and its answer never commits.
    SignInPageAsSent,
    Withhold(TokenRefusal),
}

/// The decision on a token-endpoint body. Keys and values are scanned percent-decoded, a body that
/// is not UTF-8 included: a nonce is found by its shape wherever it sits, and only the first
/// `refresh_token` of a refresh may carry one, which is then resolved, emptied or held back. What
/// leaves is always Rust's own encoding of a grant it knows.
pub(crate) fn decide(body: &[u8], ts: Option<&StoredTokenState>, now: u64) -> TokenFate {
    let pairs: Vec<(String, String)> = url::form_urlencoded::parse(body)
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    let grant_type = pairs
        .iter()
        .find(|(k, _)| k == "grant_type")
        .map(|(_, v)| v.as_str());
    let exit = RequestedGrant::of(grant_type).sdk_exit();
    let carrier = exit
        .and(pairs.iter().position(|(k, _)| k == "refresh_token"))
        .filter(|&i| is_opaque(&pairs[i].1));
    let misplaced = pairs.iter().enumerate().any(|(i, (k, v))| {
        carries_nonce_shape(k) || (Some(i) != carrier && carries_nonce_shape(v))
    });
    if misplaced {
        return TokenFate::Withhold(TokenRefusal::NonceMisplaced);
    }
    let Ok(text) = std::str::from_utf8(body) else {
        return TokenFate::Withhold(TokenRefusal::NotUtf8);
    };
    if text.bytes().any(|b| b < 0x20) {
        return TokenFate::Withhold(TokenRefusal::ControlByte);
    }
    let request = match GrantRequest::parse(text) {
        Ok(request) => request,
        Err(why) => return TokenFate::Withhold(why),
    };
    let Some(exit) = exit.filter(|_| carrier.is_some()) else {
        return TokenFate::Reissue {
            body: request.encode(),
            client_id: request.client_id().map(str::to_owned),
            origin: request.grant.unsubstituted_origin(),
            code: (request.grant == RequestedGrant::AuthorizationCode)
                .then(|| request.first("code").map(LoginProof::of_code))
                .flatten(),
        };
    };
    if let Some(rewrite) = request.substitute_refresh(ts, now) {
        return TokenFate::Substitute(rewrite);
    }
    match exit {
        SdkExit::LogsOut => TokenFate::Neutralise(request.emptied()),
        SdkExit::Retries => TokenFate::Withhold(TokenRefusal::UpdateClientUnresolved),
    }
}

/// The document a token request comes from, read on its initiator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TokenSender {
    /// A page on the sign-in hosts. Nothing of ours runs there and no nonce lives in its storage.
    /// A body it posts is not ours to read, and its shape is not known here.
    SignInPage,
    /// The desktop page, its plugins, or an initiator nothing names.
    App,
}

impl TokenSender {
    pub(crate) fn of(asker: Asker) -> Self {
        match asker {
            Asker::LoginHost => Self::SignInPage,
            Asker::Tidal | Asker::Foreign => Self::App,
        }
    }
}

/// What each sign-in route may carry, judged before any effect of the request: a logout carrying a
/// body is refused before it ends the session. The grant's body is judged with its sender.
pub(crate) fn route_body(route: AuthRoute, body: &TokenBody) -> Result<(), TokenRefusal> {
    match (route, body) {
        (AuthRoute::WebSessionLogout | AuthRoute::Profile, TokenBody::Absent) => Ok(()),
        (AuthRoute::WebSessionLogout | AuthRoute::Profile, _) => Err(TokenRefusal::UnexpectedBody),
        (AuthRoute::WebSessionCredentials, TokenBody::Bytes(_)) => Ok(()),
        (AuthRoute::WebSessionCredentials, _) => Err(TokenRefusal::Unreadable),
        (AuthRoute::TokenGrant | AuthRoute::LoginPage, _) => Ok(()),
        (AuthRoute::NotAuth | AuthRoute::Refused, _) => Err(TokenRefusal::NoRoute),
    }
}

fn any_pair_carries_a_nonce(form: &[u8]) -> bool {
    url::form_urlencoded::parse(form)
        .any(|(k, v)| carries_nonce_shape(&k) || carries_nonce_shape(&v))
}

/// The fate of a whole token-endpoint request. The endpoint reads the query before the body (a
/// nonce there is misplaced whatever the body holds). The sign-in page's requests are not ours
/// to rewrite: they leave as they came unless they carry a nonce, and never commit.
fn fate_of(
    query: Option<&str>,
    body: &TokenBody,
    sender: TokenSender,
    ts: Option<&StoredTokenState>,
    now: u64,
) -> TokenFate {
    if query.is_some_and(|q| any_pair_carries_a_nonce(q.as_bytes())) {
        return TokenFate::Withhold(TokenRefusal::NonceMisplaced);
    }
    match (sender, body) {
        (TokenSender::SignInPage, TokenBody::Bytes(bytes)) if any_pair_carries_a_nonce(bytes) => {
            TokenFate::Withhold(TokenRefusal::NonceMisplaced)
        }
        (TokenSender::SignInPage, _) => TokenFate::SignInPageAsSent,
        (TokenSender::App, TokenBody::Bytes(bytes)) => decide(bytes, ts, now),
        (TokenSender::App, TokenBody::Absent | TokenBody::Unreadable) => {
            TokenFate::Withhold(TokenRefusal::Unreadable)
        }
    }
}

/// A token-endpoint request after Rust's pass over its body.
pub(crate) struct PreparedTokenBody {
    /// The body to send in place of the page's, when Rust rewrote it.
    pub(crate) rewritten: Option<String>,
    /// The client the sent body names: a refresh token minted in reply is bound to it.
    pub(crate) client_id: Option<String>,
    /// The session the request leaves under, read in the section that picked its token: a logout
    /// between two reads would commit one account's reply into the next session.
    pub(crate) session_epoch: u64,
    /// What the answer may do once it arrives.
    pub(crate) origin: ExchangeOrigin,
    /// The sign-in return a code exchange was admitted on, spent when its answer commits.
    pub(crate) code: Option<LoginProof>,
}

/// A code exchange leaves only naming the sign-in return the main frame received: a plugin holds
/// no code of its own that TIDAL's page sent there.
fn admit_sign_in(fate: TokenFate, proves: impl FnOnce(Option<LoginProof>) -> bool) -> TokenFate {
    match fate {
        TokenFate::Reissue {
            origin: ExchangeOrigin::Code,
            code,
            ..
        } if !proves(code) => TokenFate::Withhold(TokenRefusal::NoLoginProof),
        fate => fate,
    }
}

pub(crate) enum PreparedToken {
    Send(PreparedTokenBody),
    Withhold(TokenRefusal),
}

/// The pass CEF's request filter runs on a token-endpoint request.
pub(crate) fn prepare_token_body(
    query: Option<&str>,
    body: &TokenBody,
    sender: TokenSender,
) -> PreparedToken {
    let now = now_unix_secs();
    // No AppState yet is no session: a nonce then names nothing to resolve to.
    let (fate, session_epoch) = crate::app_state::with_state(|state| {
        let fate = fate_of(query, body, sender, state.token_state.as_ref(), now);
        (
            admit_sign_in(fate, |code| state.proves_login(code)),
            state.session_epoch,
        )
    })
    .unwrap_or_else(|| {
        let fate = fate_of(query, body, sender, None, now);
        (admit_sign_in(fate, |_| false), 0)
    });
    let sent = |rewritten: Option<String>, client_id, origin, code| {
        PreparedToken::Send(PreparedTokenBody {
            rewritten,
            client_id,
            session_epoch,
            origin,
            code,
        })
    };
    match fate {
        TokenFate::Substitute(rewrite) => sent(
            Some(rewrite.body),
            rewrite.client_id,
            ExchangeOrigin::Substituted,
            None,
        ),
        TokenFate::Neutralise(emptied) => sent(Some(emptied), None, ExchangeOrigin::Inert, None),
        TokenFate::Reissue {
            body,
            client_id,
            origin,
            code,
        } => sent(Some(body), client_id, origin, code),
        TokenFate::SignInPageAsSent => sent(None, None, ExchangeOrigin::Inert, None),
        TokenFate::Withhold(why) => PreparedToken::Withhold(why),
    }
}

/// [`prepare_token_body`] on a request CEF is about to send to the token endpoint, Rust's body
/// swapped in. A body that cannot be swapped would leave as the page wrote it: held back instead.
pub(super) fn prepare_token_request(
    req: &mut Request,
    url: &RequestUrl,
    body: &TokenBody,
    sender: TokenSender,
) -> PreparedToken {
    let prepared = prepare_token_body(url.parsed().and_then(|u| u.query()), body, sender);
    if let PreparedToken::Send(PreparedTokenBody {
        rewritten: Some(new_body),
        ..
    }) = &prepared
        && !exchange(req, new_body)
    {
        return PreparedToken::Withhold(TokenRefusal::BodyNotReplaced);
    }
    prepared
}

/// Puts Rust's body and its `Content-Type` on the request, then reads both back: CEF reports no
/// failure of either. Only what reads back as written counts as sent. The URL is never touched
/// here, which would revert the body.
fn exchange(req: &mut Request, body: &str) -> bool {
    let (Some(mut post_data), Some(mut el)) = (post_data_create(), post_data_element_create())
    else {
        return false;
    };
    let bytes = body.as_bytes();
    el.set_to_bytes(bytes.len(), bytes.as_ptr());
    if post_data.add_element(Some(&mut el)) == 0 {
        return false;
    }
    req.set_post_data(Some(&mut post_data));
    if !matches!(read_token_body(req), TokenBody::Bytes(sent) if sent == bytes) {
        return false;
    }
    let name = CefString::from(CONTENT_TYPE);
    req.set_header_by_name(Some(&name), Some(&CefString::from(FORM_CONTENT_TYPE)), 1);
    userfree_to_string(&req.header_by_name(Some(&name))) == FORM_CONTENT_TYPE
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

/// A sign-in route other than the profile has no use for a bearer of ours: one carrying a nonce's
/// shape is removed, never resolved. `false` when it could not be removed.
pub(super) fn drop_nonce_bearer(req: &mut Request) -> bool {
    let header = userfree_to_string(&req.header_by_name(Some(&CefString::from(AUTHORIZATION))));
    if !carries_nonce_shape(&header) {
        return true;
    }
    crate::verr!("[AUTH] a nonce bearer on a sign-in route was removed");
    strip_header(req, AUTHORIZATION)
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

#[cfg(test)]
#[path = "../../../tests/unit/ui/token_filter/request.rs"]
mod tests;
