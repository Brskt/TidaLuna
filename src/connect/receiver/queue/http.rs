//! HTTP / OAuth layer for the queue subsystem.
//!
//! Functions here are stateless: they take the `ConnectHttp` clients
//! and the relevant `ServerInfo` / `OAuthServerInfo` and return a
//! `Result`. They never touch `QueueState`. The façade (`QueueManager`)
//! owns all state mutation; on a successful refresh it performs the
//! `AuthStore` CAS itself and then invokes `update_access_token` to keep
//! the wire-shaped `ServerInfo` in sync.

use crate::connect::consts;
use crate::connect::types::{OAuthServerInfo, ServerInfo};
use crate::ui::hosts::HostShape;

use super::QueueError;

/// Build an `Authorization` header value from a `ServerInfo`. Prefers an
/// already-formatted `header_auth`; otherwise falls back to
/// `"Bearer {oauth_parameters.access_token}"`. Handles both the top-level
/// `auth_info` and the nested `oauth_server_info.auth_info`.
pub(super) fn resolve_auth_header(server: &ServerInfo) -> String {
    if let Some(ref ai) = server.auth_info {
        if let Some(ref ha) = ai.header_auth {
            return ha.clone();
        }
        if let Some(ref params) = ai.oauth_parameters {
            return format!("Bearer {}", params.access_token);
        }
        if let Some(ref oauth_server) = ai.oauth_server_info {
            if let Some(ref ha) = oauth_server.auth_info.header_auth {
                return ha.clone();
            }
            if let Some(ref params) = oauth_server.auth_info.oauth_parameters {
                return format!("Bearer {}", params.access_token);
            }
        }
    }
    String::new()
}

/// Read `(access_token, refresh_token, scope)` from the OAuth parameters
/// buried inside a `ServerInfo`. Returns `None` if the ServerInfo does not
/// carry OAuth credentials. Prefers the outer `auth_info.oauth_parameters`
/// and falls back to the nested `oauth_server_info.auth_info.oauth_parameters`.
pub(super) fn extract_oauth_params(
    server: Option<&ServerInfo>,
) -> Option<(String, String, Option<String>)> {
    let ai = server.and_then(|s| s.auth_info.as_ref())?;
    let scope = ai
        .oauth_server_info
        .as_ref()
        .and_then(|o| o.form_parameters.as_ref())
        .map(|f| f.scope.clone());

    if let Some(p) = ai.oauth_parameters.as_ref() {
        return Some((p.access_token.clone(), p.refresh_token.clone(), scope));
    }
    if let Some(p) = ai
        .oauth_server_info
        .as_ref()
        .and_then(|o| o.auth_info.oauth_parameters.as_ref())
    {
        return Some((p.access_token.clone(), p.refresh_token.clone(), scope));
    }
    None
}

/// Parse the OAuth `error` field from a refresh-response body. Returns the
/// error code (e.g. `"invalid_grant"`, `"invalid_request"`) or `None` if
/// the body is not valid JSON or does not carry an `error` field.
fn parse_oauth_error_code(body: &[u8]) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    value
        .get("error")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// What the receiver sends to a phone-supplied server: a refresh to the token root, every queue and
/// content request to a service root. Each kind has its own client and its own hosts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RootKind {
    Token,
    Service,
}

/// A server URL a phone handed the receiver, admitted for one [`RootKind`]: https, no userinfo,
/// query or fragment, on a host of that kind. A port is admitted. The receiver attaches its token
/// to whatever it sends here: a host it does not trust never becomes a root.
#[derive(Debug, Clone)]
pub(crate) struct ConnectRoot {
    url: url::Url,
    kind: RootKind,
}

/// Why a phone's server URL is no root, logged beside the host: that pair is what adding a real
/// phone's host to the lists takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RootFault {
    Unparseable,
    NotHttps,
    Userinfo,
    Query,
    Fragment,
    /// No host of the root's kind.
    Host,
}

impl From<RootFault> for QueueError {
    fn from(_: RootFault) -> Self {
        Self::UntrustedServer
    }
}

impl ConnectRoot {
    pub(crate) fn admit(raw: &str, kind: RootKind) -> Result<Self, RootFault> {
        let url = url::Url::parse(raw).map_err(|_| RootFault::Unparseable)?;
        let host_of_kind = match kind {
            RootKind::Token => crate::state::is_token_host,
            RootKind::Service => crate::state::is_service_host,
        };
        let fault = if url.scheme() != "https" {
            Some(RootFault::NotHttps)
        } else if !url.username().is_empty() || url.password().is_some() {
            Some(RootFault::Userinfo)
        } else if url.query().is_some() {
            Some(RootFault::Query)
        } else if url.fragment().is_some() {
            Some(RootFault::Fragment)
        } else if !matches!(HostShape::of(&url), HostShape::Name(name) if host_of_kind(name)) {
            Some(RootFault::Host)
        } else {
            None
        };
        match fault {
            Some(fault) => Err(fault),
            None => Ok(Self { url, kind }),
        }
    }

    /// [`Self::admit`] at a call site: a refusal is logged ungated with the host alone, never the
    /// URL, whose query or userinfo a phone could fill. The phone hears a fixed refusal.
    pub(super) fn admit_logged(raw: &str, kind: RootKind) -> Result<Self, QueueError> {
        Self::admit(raw, kind).map_err(|fault| {
            crate::verr!(
                "[connect::queue] {kind:?} server refused ({fault:?}), host {}",
                crate::util::refused_host(raw)
            );
            QueueError::from(fault)
        })
    }

    /// The root itself, where a refresh is posted.
    pub(super) fn here(&self) -> RootedUrl {
        RootedUrl {
            url: self.url.clone(),
            kind: self.kind,
        }
    }

    /// The root with `segments` appended, each its own path segment whatever it holds, and `query`
    /// set. An id names one resource: a segment that is empty, blank, `.` or `..` is refused, and
    /// the built path must hold exactly the root's segments plus these, each reading back as given.
    pub(super) fn url(
        &self,
        segments: &[&str],
        query: &[(&str, &str)],
    ) -> Result<RootedUrl, QueueError> {
        if let Some(index) = segments.iter().position(|s| {
            let core = s.trim_matches(|c: char| c.is_ascii_whitespace() || c.is_ascii_control());
            matches!(core, "" | "." | "..")
        }) {
            return Err(QueueError::RefusedSegment(index));
        }
        let root: Vec<String> = self
            .url
            .path_segments()
            .into_iter()
            .flatten()
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        let mut url = self.url.clone();
        url.path_segments_mut()
            .map_err(|()| QueueError::UntrustedServer)?
            .clear()
            .extend(&root)
            .extend(segments);
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query);
        }

        let built: Vec<&str> = url.path_segments().into_iter().flatten().collect();
        if built.len() != root.len() + segments.len() {
            return Err(QueueError::RefusedSegment(0));
        }
        for (index, (got, given)) in built[root.len()..].iter().zip(segments).enumerate() {
            if percent_encoding::percent_decode_str(got)
                .decode_utf8()
                .ok()
                .as_deref()
                != Some(given)
            {
                return Err(QueueError::RefusedSegment(index));
            }
        }
        Ok(RootedUrl {
            url,
            kind: self.kind,
        })
    }
}

/// A URL built from a [`ConnectRoot`], carrying the kind that picks its client.
#[derive(Debug, Clone)]
pub(super) struct RootedUrl {
    pub(super) url: url::Url,
    kind: RootKind,
}

/// The receiver's two clients, built once: a client per request cost a TLS handshake each.
#[derive(Clone)]
pub(crate) struct ConnectHttp {
    auth: reqwest::Client,
    service: reqwest::Client,
}

impl ConnectHttp {
    pub(crate) fn production() -> Self {
        Self {
            auth: crate::state::AUTH_CLIENT.clone(),
            service: crate::state::SERVICE_CLIENT.clone(),
        }
    }

    #[cfg(test)]
    pub(crate) fn new(auth: reqwest::Client, service: reqwest::Client) -> Self {
        Self { auth, service }
    }

    pub(super) fn get(&self, target: &RootedUrl) -> reqwest::RequestBuilder {
        self.client(target.kind).get(target.url.clone())
    }

    fn post(&self, target: &RootedUrl) -> reqwest::RequestBuilder {
        self.client(target.kind).post(target.url.clone())
    }

    fn client(&self, kind: RootKind) -> &reqwest::Client {
        match kind {
            RootKind::Token => &self.auth,
            RootKind::Service => &self.service,
        }
    }
}

/// A refused redirect hop is told by the error's chain, never by its text.
pub(super) fn send_error(e: reqwest::Error) -> QueueError {
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(&e);
    while let Some(cause) = source {
        if cause.is::<crate::util::Refused>() {
            return QueueError::RedirectRefused;
        }
        source = cause.source();
    }
    QueueError::Network(e.to_string())
}

/// GET a JSON resource on the queue server, using the server's auth header.
pub(super) async fn get_with_auth(
    http: &ConnectHttp,
    queue_server: Option<&ServerInfo>,
    target: &RootedUrl,
) -> Result<serde_json::Value, QueueError> {
    let server = queue_server.ok_or(QueueError::NoServer)?;
    let auth_header = resolve_auth_header(server);

    let response = http
        .get(target)
        .header("Authorization", &auth_header)
        .timeout(std::time::Duration::from_secs(consts::HTTP_TIMEOUT_SECS))
        .send()
        .await
        .map_err(send_error)?;

    let status = response.status();
    if status.as_u16() == 401 {
        return Err(QueueError::TokenExpired);
    }
    if !status.is_success() {
        return Err(QueueError::HttpStatus(status.as_u16()));
    }

    response
        .json::<serde_json::Value>()
        .await
        .map_err(|e| QueueError::InvalidResponse(e.to_string()))
}

/// POST a JSON body to the queue server. The response body is discarded;
/// only HTTP success is reported.
pub(super) async fn post_with_auth(
    http: &ConnectHttp,
    queue_server: Option<&ServerInfo>,
    target: &RootedUrl,
    body: &serde_json::Value,
) -> Result<(), QueueError> {
    let server = queue_server.ok_or(QueueError::NoServer)?;
    let auth_header = resolve_auth_header(server);

    http.post(target)
        .header("Authorization", &auth_header)
        .json(body)
        .timeout(std::time::Duration::from_secs(consts::HTTP_TIMEOUT_SECS))
        .send()
        .await
        .map_err(send_error)?;
    Ok(())
}

/// Successful OAuth refresh outcome. The server may rotate the refresh
/// token (OAuth 2.0 §6): a new one is reported when present.
pub(super) struct RefreshSuccess {
    pub access_token: String,
    pub refresh_token: Option<String>,
}

/// POST an OAuth `grant_type=refresh_token` request. Stateless: the caller
/// is responsible for taking the current `refresh_token` from the
/// `AuthStore` snapshot and for installing the new credentials via a CAS
/// afterwards.
///
/// Classifies server responses:
/// * 2xx with `access_token` -> `Ok(RefreshSuccess)`
/// * 4xx/5xx carrying `error: "invalid_grant"` -> `Err(AuthTerminated)`
/// * Other non-success statuses -> `Err(HttpStatus)`
/// * Network / transport errors -> `Err(Network)`
/// * Parse errors -> `Err(InvalidResponse)`
pub(super) async fn refresh_token(
    http: &ConnectHttp,
    oauth: &OAuthServerInfo,
    current_refresh_token: &str,
) -> Result<RefreshSuccess, QueueError> {
    // The phone names where the refresh goes: only TIDAL's token host receives it.
    let target = ConnectRoot::admit_logged(&oauth.server_url, RootKind::Token)?.here();
    let auth_header = oauth
        .auth_info
        .header_auth
        .as_deref()
        .unwrap_or("")
        .to_string();

    let mut form = std::collections::HashMap::new();
    if let Some(ref fp) = oauth.form_parameters {
        form.insert("grant_type".to_string(), fp.grant_type.clone());
        form.insert("scope".to_string(), fp.scope.clone());
    }
    form.insert(
        "refresh_token".to_string(),
        current_refresh_token.to_string(),
    );
    let resp = http
        .post(&target)
        .header("Authorization", &auth_header)
        .form(&form)
        .timeout(std::time::Duration::from_secs(consts::HTTP_TIMEOUT_SECS))
        .send()
        .await
        .map_err(send_error)?;

    let status = resp.status();
    let body_bytes = resp
        .bytes()
        .await
        .map_err(|e| QueueError::Network(e.to_string()))?;

    if !status.is_success() {
        if let Some(err_code) = parse_oauth_error_code(&body_bytes)
            && err_code == "invalid_grant"
        {
            return Err(QueueError::AuthTerminated {
                provider_error: err_code,
            });
        }
        return Err(QueueError::HttpStatus(status.as_u16()));
    }

    let body: serde_json::Value = serde_json::from_slice(&body_bytes)
        .map_err(|e| QueueError::InvalidResponse(e.to_string()))?;

    let access_token = body
        .get("access_token")
        .and_then(|v| v.as_str())
        .ok_or(QueueError::MissingField("access_token"))?
        .to_string();

    let rotated_refresh = body
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    Ok(RefreshSuccess {
        access_token,
        refresh_token: rotated_refresh,
    })
}

#[cfg(test)]
#[path = "../../../../tests/unit/connect/receiver/queue/http.rs"]
mod tests;
