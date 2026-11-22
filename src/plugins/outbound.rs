//! A plugin's outbound request, admitted once: every route (`plugin.fetch`, `tidal.fetch`, a native
//! plugin's `net.fetch`) builds the same [`OutboundRequest`] from what the plugin sent, or refuses.
//!
//! Nothing is guessed. A verb outside the seven, a header value that is not a string, a body on a
//! GET or HEAD: each is a refusal, never a fallback to GET or a dropped field.

use base64::Engine;
use reqwest::header::{HeaderName, HeaderValue};
use serde_json::Value;

use crate::ui::hosts::{EgressRefusal, PermittedUrl};

/// The verbs a plugin may send, matched without regard to ASCII case and without trimming.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FetchMethod {
    Delete,
    Get,
    Head,
    Options,
    Patch,
    Post,
    Put,
}

impl FetchMethod {
    const ALL: [Self; 7] = [
        Self::Delete,
        Self::Get,
        Self::Head,
        Self::Options,
        Self::Patch,
        Self::Post,
        Self::Put,
    ];

    fn parse(raw: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|method| method.as_str().eq_ignore_ascii_case(raw))
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Delete => "DELETE",
            Self::Get => "GET",
            Self::Head => "HEAD",
            Self::Options => "OPTIONS",
            Self::Patch => "PATCH",
            Self::Post => "POST",
            Self::Put => "PUT",
        }
    }

    fn to_reqwest(self) -> reqwest::Method {
        match self {
            Self::Delete => reqwest::Method::DELETE,
            Self::Get => reqwest::Method::GET,
            Self::Head => reqwest::Method::HEAD,
            Self::Options => reqwest::Method::OPTIONS,
            Self::Patch => reqwest::Method::PATCH,
            Self::Post => reqwest::Method::POST,
            Self::Put => reqwest::Method::PUT,
        }
    }
}

/// An empty string is a body: `fetch(url, { body: '' })` sends one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FetchBody {
    Absent,
    Present(Vec<u8>),
}

/// What makes a request's shape unsendable, past its URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ShapeError {
    /// The options are not an object, or a field has the wrong type.
    Options(&'static str),
    /// Outside the seven verbs, kept escaped and cut to 16 characters.
    Method(String),
    BodyOnGetOrHead,
    HeaderName,
    HeaderValue,
    /// `Content-Length` or `Transfer-Encoding`: the framing is reqwest's to write.
    FramingHeader,
    /// Over the cap, or not base64.
    Body(&'static str),
}

/// Why a plugin request was refused before anything was sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Refusal {
    Url(EgressRefusal),
    Shape(ShapeError),
}

impl Refusal {
    pub(crate) fn status(&self) -> u16 {
        match self {
            Self::Url(_) => 403,
            Self::Shape(_) => 400,
        }
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Url(refusal) => refusal.fmt(f),
            Self::Shape(ShapeError::Options(what)) => {
                write!(f, "the request options are malformed: {what}")
            }
            Self::Shape(ShapeError::Method(method)) => {
                write!(f, "the method {method} is not allowed")
            }
            Self::Shape(ShapeError::BodyOnGetOrHead) => {
                f.write_str("a GET or HEAD request carries no body")
            }
            Self::Shape(ShapeError::HeaderName) => f.write_str("a header name is not valid"),
            Self::Shape(ShapeError::HeaderValue) => {
                f.write_str("a header value is not a valid string")
            }
            Self::Shape(ShapeError::FramingHeader) => {
                f.write_str("Content-Length and Transfer-Encoding are not the plugin's to set")
            }
            Self::Shape(ShapeError::Body(what)) => write!(f, "the request body {what}"),
        }
    }
}

/// Max request body a native plugin may send, checked on its base64 length before any decode.
pub(crate) const MAX_BODY_BYTES: usize = 25 * 1024 * 1024;

/// A request a plugin may send: its URL judged, its shape whole.
#[derive(Debug)]
pub(crate) struct OutboundRequest {
    url: PermittedUrl,
    method: FetchMethod,
    headers: Vec<(HeaderName, HeaderValue)>,
    body: FetchBody,
}

impl OutboundRequest {
    /// From `plugin.fetch`, `tidal.fetch` and `auth_fetch`: options as `{method, headers, body}`,
    /// headers as an object. A missing method is a GET; a `null` one is refused.
    pub(crate) fn from_object(url: &str, opts_json: &str) -> Result<Self, Refusal> {
        let opts: Option<serde_json::Map<String, Value>> = serde_json::from_str(opts_json).ok();
        let headers: Vec<(&str, &Value)> = match opts.as_ref().and_then(|o| o.get("headers")) {
            Some(Value::Object(map)) => map.iter().map(|(k, v)| (k.as_str(), v)).collect(),
            _ => Vec::new(),
        };
        admit(url, &headers, || {
            let opts = opts
                .as_ref()
                .ok_or(ShapeError::Options("not a JSON object"))?;
            if !matches!(opts.get("headers"), None | Some(Value::Object(_))) {
                return Err(ShapeError::Options("headers is not an object"));
            }
            let body = match opts.get("body") {
                None | Some(Value::Null) => FetchBody::Absent,
                Some(Value::String(text)) => FetchBody::Present(text.as_bytes().to_vec()),
                Some(_) => return Err(ShapeError::Options("body is not a string")),
            };
            Ok((opts.get("method"), body))
        })
    }

    /// From a native plugin's `net.fetch` line: headers as `[name, value]` pairs, the body base64.
    pub(crate) fn from_pairs(req: &Value) -> Result<Self, Refusal> {
        let url = match req.get("url") {
            Some(Value::String(url)) => url.as_str(),
            _ => "",
        };
        let pairs: Vec<(&str, &Value)> = match req.get("headers") {
            Some(Value::Array(pairs)) => pairs
                .iter()
                .filter_map(|pair| match pair.as_array()?.as_slice() {
                    [Value::String(name), value] => Some((name.as_str(), value)),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        };
        admit(url, &pairs, || {
            let well_formed = match req.get("headers") {
                None | Some(Value::Null) => true,
                Some(Value::Array(all)) => all.len() == pairs.len(),
                Some(_) => false,
            };
            if !well_formed {
                return Err(ShapeError::Options("headers are not [name, value] pairs"));
            }
            let body = match req.get("body") {
                None | Some(Value::Null) => FetchBody::Absent,
                Some(Value::String(b64)) => {
                    // On the encoded length, before the decode allocates.
                    if b64.len() / 4 * 3 > MAX_BODY_BYTES {
                        return Err(ShapeError::Body("exceeds the limit"));
                    }
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(b64)
                        .map_err(|_| ShapeError::Body("is not base64"))?;
                    FetchBody::Present(bytes)
                }
                Some(_) => return Err(ShapeError::Options("body is not a string")),
            };
            Ok((req.get("method"), body))
        })
    }

    pub(crate) fn url(&self) -> &url::Url {
        self.url.as_url()
    }

    pub(crate) fn method(&self) -> FetchMethod {
        self.method
    }

    pub(crate) fn has_header(&self, name: &str) -> bool {
        self.headers
            .iter()
            .any(|(n, _)| n.as_str().eq_ignore_ascii_case(name))
    }

    /// The request on `client`, every field as admitted.
    pub(crate) fn build(self, client: &reqwest::Client) -> reqwest::RequestBuilder {
        let mut builder = client.request(self.method.to_reqwest(), self.url.into_url());
        for (name, value) in self.headers {
            builder = builder.header(name, value);
        }
        match self.body {
            FetchBody::Present(bytes) => builder.body(bytes),
            FetchBody::Absent => builder,
        }
    }
}

/// The one admission. The URL is judged first, with a forged `Host` counted as a URL refusal: it
/// names a destination. The shape follows; `rest` reads the method field and the body.
fn admit<'v>(
    url: &str,
    headers: &[(&str, &Value)],
    rest: impl FnOnce() -> Result<(Option<&'v Value>, FetchBody), ShapeError>,
) -> Result<OutboundRequest, Refusal> {
    let parsed = url::Url::parse(url).map_err(|_| Refusal::Url(EgressRefusal::Unparseable))?;
    if !matches!(parsed.scheme(), "https" | "http") {
        return Err(Refusal::Url(EgressRefusal::NotHttp));
    }
    let url = PermittedUrl::new(parsed).map_err(Refusal::Url)?;
    if headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("host"))
    {
        return Err(Refusal::Url(EgressRefusal::ForgedHostHeader));
    }

    let (method, body) = rest().map_err(Refusal::Shape)?;
    let method = match method {
        None => FetchMethod::Get,
        Some(Value::String(raw)) => FetchMethod::parse(raw.as_str()).ok_or_else(|| {
            Refusal::Shape(ShapeError::Method(raw.escape_default().take(16).collect()))
        })?,
        Some(_) => {
            return Err(Refusal::Shape(ShapeError::Options(
                "method is not a string",
            )));
        }
    };

    let mut admitted = Vec::with_capacity(headers.len());
    for (name, value) in headers {
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| Refusal::Shape(ShapeError::HeaderName))?;
        if name == reqwest::header::CONTENT_LENGTH || name == reqwest::header::TRANSFER_ENCODING {
            return Err(Refusal::Shape(ShapeError::FramingHeader));
        }
        let value = value
            .as_str()
            .and_then(|text| HeaderValue::from_str(text).ok())
            .ok_or(Refusal::Shape(ShapeError::HeaderValue))?;
        admitted.push((name, value));
    }

    if matches!(method, FetchMethod::Get | FetchMethod::Head) && body != FetchBody::Absent {
        return Err(Refusal::Shape(ShapeError::BodyOnGetOrHead));
    }
    Ok(OutboundRequest {
        url,
        method,
        headers: admitted,
        body,
    })
}

/// Why a plugin request that was sent did not come back.
#[derive(Debug)]
pub(crate) enum PluginFetchError {
    /// A redirect reached a host no plugin request may reach.
    Refused(EgressRefusal),
    Transport(String),
    Body(String),
}

impl PluginFetchError {
    /// A refused redirect is told by the error's chain, never by its text.
    pub(crate) fn of_send(e: reqwest::Error) -> Self {
        let mut source: Option<&(dyn std::error::Error + 'static)> = Some(&e);
        while let Some(cause) = source {
            if cause.is::<crate::util::Refused>() {
                return Self::Refused(EgressRefusal::RedirectedToRefused);
            }
            source = cause.source();
        }
        Self::Transport(crate::util::network_error_text(e))
    }
}

impl std::fmt::Display for PluginFetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(refusal) => write!(f, "refused: {refusal}"),
            Self::Transport(text) => write!(f, "failed: {text}"),
            Self::Body(text) => write!(f, "body read failed: {text}"),
        }
    }
}

#[cfg(test)]
#[path = "../../tests/unit/plugins/outbound.rs"]
mod tests;
