use url::Url;

use crate::ui::hosts::{HostShape, TidalDomain};

pub(crate) const HOST_DESKTOP: &str = "desktop.tidal.com";
pub(crate) const HOST_LOGIN: &str = "login.tidal.com";
pub(crate) const HOST_AUTH: &str = "auth.tidal.com";
pub(crate) const HOST_API: &str = "api.tidal.com";
pub(crate) const HOST_API_HIFI: &str = "api.tidalhifi.com";
pub(crate) const HOST_QUEUE: &str = "connectqueue.tidal.com";
pub(crate) const PATH_QUEUE: &str = "/v1/queues";
pub(crate) const PATH_CONTENT: &str = "/v1";
/// The hosts a Connect phone's queue and content servers may name, and the only ones the service
/// client follows a redirect to. Known from TIDAL's own server info; not proven complete.
pub(crate) const SERVICE_HOSTS: [&str; 3] = [HOST_API, HOST_QUEUE, HOST_API_HIFI];
pub(crate) const REDIRECT_URI: &str = "tidal://login/auth";
/// Where the desktop page receives our callback.
const PATH_LOGIN_CALLBACK: &str = "/login/auth";
/// TIDAL's OAuth token endpoint, under the `/v1/` API prefix. The version-less
/// `/oauth2/token` path answers 403 to a POST; only `/v1/oauth2/token` is the
/// real grant endpoint (the SDK's own login/refresh use it too).
pub(crate) const PATH_OAUTH_TOKEN: &str = "/v1/oauth2/token";
/// The prefix of every route of TIDAL's auth API, `oauthAuthUrl` in the bundle.
const AUTH_API_PREFIX: &str = "/v1/";
const PATH_PROFILE: &str = "/oauth2/me";

/// Who asks, read on the request's initiator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Asker {
    /// A page on the sign-in hosts.
    LoginHost,
    /// TIDAL's desktop page, its sign-in pages and our callback.
    Tidal,
    /// Anything else, or an initiator nothing names.
    Foreign,
}

impl Asker {
    /// The origin of the document that made the request. CEF's frame is no witness: it falls back
    /// to the main frame (a script of `login.tidal.com` then reads as `DesktopApp`). A
    /// navigation carries `"null"` and a browser-made request none, both foreign.
    pub(crate) fn of_initiator(initiator: Option<&str>) -> Self {
        match initiator.map(PageKind::classify) {
            Some(PageKind::AuthHost) => Self::LoginHost,
            Some(PageKind::DesktopApp | PageKind::LoginPage | PageKind::LoginCallback) => {
                Self::Tidal
            }
            Some(PageKind::External | PageKind::TidalCallback) | None => Self::Foreign,
        }
    }
}

/// What a request to TIDAL's sign-in hosts is, by its exact route. Paths are matched raw: no
/// alias, percent-encoding or extra slash reaches the same server route under another name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuthRoute {
    /// Not a sign-in host.
    NotAuth,
    /// The sign-in page's own traffic.
    LoginPage,
    /// `POST https://auth.tidal.com/v1/oauth2/token`.
    TokenGrant,
    /// `POST https://auth.tidal.com/v1/web-session/credentials`.
    WebSessionCredentials,
    /// `POST https://auth.tidal.com/v1/web-session/logout`.
    WebSessionLogout,
    /// `GET https://login.tidal.com/oauth2/me`.
    Profile,
    /// Anything else on a sign-in host, a malformed TIDAL host, or a foreign asker.
    Refused,
}

impl AuthRoute {
    pub(crate) fn of(method: &str, url: &Url, asker: Asker) -> Self {
        match crate::ui::hosts::HostVerdict::of(url) {
            crate::ui::hosts::HostVerdict::Open => return Self::NotAuth,
            crate::ui::hosts::HostVerdict::MalformedTidal => return Self::Refused,
            crate::ui::hosts::HostVerdict::SignIn => {}
        }
        if asker == Asker::Foreign {
            return Self::Refused;
        }
        let plain_origin = url.scheme() == "https"
            && url.username().is_empty()
            && url.password().is_none()
            && url.port().is_none();
        let plain =
            plain_origin && url.fragment().is_none() && url.query().is_none_or(str::is_empty);
        match url.host_str() {
            // The sign-in page's own requests carry their query, as in `/api/email?...`.
            Some(HOST_LOGIN) if asker == Asker::LoginHost && plain_origin => Self::LoginPage,
            _ if !plain => Self::Refused,
            Some(HOST_AUTH) => match (method, url.path().strip_prefix(AUTH_API_PREFIX)) {
                ("POST", Some("oauth2/token")) => Self::TokenGrant,
                ("POST", Some("web-session/credentials")) => Self::WebSessionCredentials,
                ("POST", Some("web-session/logout")) => Self::WebSessionLogout,
                _ => Self::Refused,
            },
            Some(HOST_LOGIN) if method == "GET" && url.path() == PATH_PROFILE => Self::Profile,
            _ => Self::Refused,
        }
    }

    /// The one request whose sending says the user meant to leave the session.
    pub(crate) fn ends_session(self) -> bool {
        self == Self::WebSessionLogout
    }
}

/// The return of a sign-in through TIDAL's own page, named by the digest of the code it carried.
/// Only a code exchange whose code it names may commit a sign-in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LoginProof([u8; 32]);

impl LoginProof {
    pub(crate) fn of_code(code: &str) -> Self {
        use sha2::{Digest, Sha256};
        Self(Sha256::digest(code.as_bytes()).into())
    }

    /// Our callback, reached in the main frame from a page of `login.tidal.com` itself, carrying
    /// exactly one code that decodes to no further escape. `callback` is the `tidal://` return as
    /// the desktop page reloads it; `frame_url` is the page that sent it there.
    pub(crate) fn observe(
        callback: &str,
        frame_url: Option<&str>,
        is_main_frame: bool,
    ) -> Option<Self> {
        let sender = Url::parse(frame_url?).ok()?;
        let from_login = sender.scheme() == "https"
            && matches!(HostShape::of(&sender), HostShape::Name(name) if name.is(HOST_LOGIN));
        let callback = Url::parse(callback).ok()?;
        if !is_main_frame
            || !from_login
            || callback.host_str() != Some(HOST_DESKTOP)
            || callback.path() != PATH_LOGIN_CALLBACK
        {
            return None;
        }
        let mut codes = callback.query_pairs().filter(|(key, _)| key == "code");
        match (codes.next(), codes.next()) {
            (Some((_, code)), None) if !code.is_empty() && !code.contains('%') => {
                Some(Self::of_code(&code))
            }
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PageKind {
    DesktopApp,
    LoginPage,
    LoginCallback,
    AuthHost,
    TidalCallback,
    External,
}

impl PageKind {
    pub(crate) fn classify(raw: &str) -> Self {
        let Ok(url) = Url::parse(raw) else {
            return Self::External;
        };

        if url.scheme() == "tidal" {
            return Self::TidalCallback;
        }
        // A plaintext page on a TIDAL host is whatever the network in between served.
        if url.scheme() != "https" {
            return Self::External;
        }

        let host = url.host_str().unwrap_or("");
        let path = url.path();

        match host {
            HOST_LOGIN | HOST_AUTH => Self::AuthHost,
            HOST_DESKTOP => {
                if path == "/login/auth" {
                    Self::LoginCallback
                } else if path.starts_with("/login") {
                    // starts_with: TIDAL uses sub-paths like /login/email
                    Self::LoginPage
                } else {
                    Self::DesktopApp
                }
            }
            _ => Self::External,
        }
    }

    /// The one trust rule for a requesting frame: every kind but `External` is TIDAL's own page
    /// or our callback. Privileged IPC and the cover scheme both read it and cannot drift.
    pub(crate) fn is_trusted(self) -> bool {
        !matches!(self, Self::External)
    }
}

pub(crate) struct NavigationPolicy {
    pub inject_early_runtime: bool,
    pub inject_init_script: bool,
    pub inject_bundle: bool,
    pub bypass_router: bool,
}

impl NavigationPolicy {
    pub(crate) fn for_page(kind: PageKind) -> Self {
        match kind {
            PageKind::DesktopApp | PageKind::LoginCallback => Self {
                inject_early_runtime: true,
                inject_init_script: true,
                inject_bundle: true,
                bypass_router: false,
            },
            // TIDAL expects bundle surfaces even on /login (session setup, nativeInterface).
            PageKind::LoginPage => Self {
                inject_early_runtime: true,
                inject_init_script: true,
                inject_bundle: true,
                bypass_router: false,
            },
            PageKind::AuthHost => Self {
                inject_early_runtime: true,
                inject_init_script: true,
                inject_bundle: false,
                bypass_router: true,
            },
            PageKind::TidalCallback => Self {
                inject_early_runtime: false,
                inject_init_script: false,
                inject_bundle: false,
                bypass_router: true,
            },
            PageKind::External => Self {
                inject_early_runtime: false,
                inject_init_script: false,
                inject_bundle: false,
                bypass_router: false,
            },
        }
    }
}

/// A request URL materialized at a CEF or IPC boundary: parsed once, the raw
/// string kept for the helpers whose contract survives an unparseable URL.
pub(crate) struct RequestUrl {
    raw: String,
    parsed: Option<Url>,
}

impl RequestUrl {
    pub(crate) fn new(raw: String) -> Self {
        let parsed = Url::parse(&raw).ok();
        Self { raw, parsed }
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.raw
    }

    pub(crate) fn parsed(&self) -> Option<&Url> {
        self.parsed.as_ref()
    }

    /// `None` when the URL does not parse: that says nothing about a host.
    pub(crate) fn shape(&self) -> Option<HostShape<'_>> {
        self.parsed.as_ref().map(HostShape::of)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.raw.is_empty()
    }
}

/// Broad check: does the URL belong to `tidal.com` or one of its subdomains? Used by the
/// exfiltration guard to tell TIDAL traffic from external. A URL that does not parse names no
/// host and is refused: every caller hands an absolute URL.
pub(crate) fn is_tidal_origin(url: &RequestUrl) -> bool {
    matches!(url.shape(), Some(HostShape::Name(name)) if name.domain() == Some(TidalDomain::Tidal))
}

/// May a proxy request (`proxy.head`) go to this URL? Destination and transport both have to hold:
/// reqwest never travels through Blink, and nothing upgrades a plaintext scheme on the way out. An
/// unparseable URL carries no transport to judge, and reqwest reads that same string with that same
/// parser.
pub(crate) fn is_secure_tidal_target(url: &RequestUrl) -> bool {
    if !is_tidal_origin(url) {
        return false;
    }
    url.parsed()
        .is_some_and(|parsed| parsed.scheme() == "https")
}

/// Hosts allowed to serve an image. A label-boundary suffix, never a substring:
/// `eviltidal.com` and `tidal.com.evil.io` are registrable by anyone and both carry the
/// allowed string. Single source of truth for the image gate, which the browser, context
/// and dialog dispatches all consult; the lists cannot drift.
const IMAGE_HOSTS: [&str; 5] = [
    "tidal.com",
    "gravatar.com",
    "github.com",
    "githubusercontent.com",
    "captcha-delivery.com",
];

/// An unparseable URL is refused rather than trusted: it says nothing about where it points.
/// Destination only: the transport half is `is_secure_image_target`'s, the same split as
/// `is_tidal_origin` and `is_secure_tidal_target`.
pub(crate) fn is_allowed_image_host(url: &RequestUrl) -> bool {
    matches!(url.shape(), Some(HostShape::Name(name)) if IMAGE_HOSTS.iter().any(|domain| name.within(domain)))
}

/// Destination and transport both: a plaintext image on an allowed host is whatever the
/// network in between makes it. Blink upgrades one on the app's own page first; the dialogs
/// render in a `data:` document, whose opaque origin restricts no mixed content, and the trust
/// prompt shows an avatar from a plugin manifest.
pub(crate) fn is_secure_image_target(url: &RequestUrl) -> bool {
    is_allowed_image_host(url)
        && url
            .parsed()
            .is_some_and(|parsed| parsed.scheme() == "https")
}

/// TIDAL API hosts that receive the injected OAuth bearer. Single source of truth for
/// `is_tidal_api` and `should_rewrite_token`; the lists cannot drift.
pub(crate) fn is_tidal_api_host(host: &str) -> bool {
    matches!(
        host,
        "api.tidal.com"
            | "api.tidalhifi.com"
            | "listen.tidal.com"
            | "desktop.tidal.com"
            | "openapi.tidal.com"
    )
}

#[cfg(test)]
#[path = "../../tests/unit/ui/nav.rs"]
mod tests;
