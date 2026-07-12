use url::Url;

pub(crate) const HOST_DESKTOP: &str = "desktop.tidal.com";
pub(crate) const HOST_LOGIN: &str = "login.tidal.com";
pub(crate) const HOST_AUTH: &str = "auth.tidal.com";
pub(crate) const HOST_API: &str = "api.tidal.com";
pub(crate) const REDIRECT_URI: &str = "tidal://login/auth";
/// TIDAL's OAuth token endpoint, under the `/v1/` API prefix. The version-less
/// `/oauth2/token` path answers 403 to a POST; only `/v1/oauth2/token` is the
/// real grant endpoint (the SDK's own login/refresh use it too).
pub(crate) const PATH_OAUTH_TOKEN: &str = "/v1/oauth2/token";

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

    pub(crate) fn is_empty(&self) -> bool {
        self.raw.is_empty()
    }
}

/// Broad check: does the URL belong to a Tidal-owned domain?
/// Used by the exfiltration guard to distinguish Tidal traffic from external.
pub(crate) fn is_tidal_origin(url: &RequestUrl) -> bool {
    let Some(parsed) = url.parsed() else {
        // Unparseable but relative stays same-origin: proxy.fetch callers pass
        // bare paths. Anything else stays external.
        return url.as_str().starts_with('/');
    };
    let host = parsed.host_str().unwrap_or("");
    host == "tidal.com" || host.ends_with(".tidal.com")
}

pub(crate) fn is_token_endpoint(url: &RequestUrl) -> bool {
    let Some(parsed) = url.parsed() else {
        return false;
    };
    let host = parsed.host_str().unwrap_or("");
    // Broad substring, decoupled from PATH_OAUTH_TOKEN's exact value: this must
    // catch whatever the SDK posts (any API-version prefix), while do_refresh
    // posts to the precise PATH_OAUTH_TOKEN.
    (host == HOST_AUTH || host == HOST_LOGIN) && parsed.path().contains("oauth2/token")
}

/// TIDAL API hosts that receive the injected OAuth bearer. Single source of truth for
/// `is_tidal_api`, `needs_auto_injection`, `should_rewrite_token` so the lists can't drift.
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
mod tests {
    use super::*;

    fn u(s: &str) -> RequestUrl {
        RequestUrl::new(s.to_string())
    }

    #[test]
    fn tidal_origin_matches_the_apex_and_subdomains() {
        assert!(is_tidal_origin(&u("https://tidal.com/")));
        assert!(is_tidal_origin(&u("https://listen.tidal.com/v1/x")));
        assert!(!is_tidal_origin(&u("https://eviltidal.com/")));
        assert!(!is_tidal_origin(&u("https://tidal.com.evil.io/")));
    }

    #[test]
    fn tidal_origin_keeps_the_relative_path_fallback() {
        assert!(is_tidal_origin(&u("/v1/tracks/1")));
        assert!(!is_tidal_origin(&u("not a url")));
    }

    #[test]
    fn token_endpoint_requires_auth_host_and_oauth_path() {
        assert!(is_token_endpoint(&u(
            "https://auth.tidal.com/v1/oauth2/token"
        )));
        assert!(is_token_endpoint(&u(
            "https://login.tidal.com/oauth2/token"
        )));
        assert!(!is_token_endpoint(&u("https://auth.tidal.com/v1/other")));
        assert!(!is_token_endpoint(&u(
            "https://api.tidal.com/v1/oauth2/token"
        )));
        assert!(!is_token_endpoint(&u("not a url")));
    }
}
