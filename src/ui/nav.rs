use url::Url;

pub(crate) const HOST_DESKTOP: &str = "desktop.tidal.com";
pub(crate) const HOST_LOGIN: &str = "login.tidal.com";
pub(crate) const HOST_AUTH: &str = "auth.tidal.com";
pub(crate) const HOST_API: &str = "api.tidal.com";
pub(crate) const REDIRECT_URI: &str = "tidal://login/auth";
pub(crate) const PATH_OAUTH_TOKEN: &str = "/oauth2/token";

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

#[allow(dead_code)]
pub(crate) struct NavigationPolicy {
    pub inject_early_runtime: bool,
    pub inject_init_script: bool,
    pub inject_bundle: bool,
    pub bypass_router: bool,
    pub attach_resource_handler: bool,
}

impl NavigationPolicy {
    pub(crate) fn for_page(kind: PageKind) -> Self {
        match kind {
            PageKind::DesktopApp | PageKind::LoginCallback => Self {
                inject_early_runtime: true,
                inject_init_script: true,
                inject_bundle: true,
                bypass_router: false,
                attach_resource_handler: true,
            },
            // TIDAL expects bundle surfaces even on /login (session setup, nativeInterface).
            PageKind::LoginPage => Self {
                inject_early_runtime: true,
                inject_init_script: true,
                inject_bundle: true,
                bypass_router: false,
                attach_resource_handler: true,
            },
            PageKind::AuthHost => Self {
                inject_early_runtime: true,
                inject_init_script: true,
                inject_bundle: false,
                bypass_router: true,
                attach_resource_handler: false,
            },
            PageKind::TidalCallback => Self {
                inject_early_runtime: false,
                inject_init_script: false,
                inject_bundle: false,
                bypass_router: true,
                attach_resource_handler: false,
            },
            PageKind::External => Self {
                inject_early_runtime: false,
                inject_init_script: false,
                inject_bundle: false,
                bypass_router: false,
                attach_resource_handler: false,
            },
        }
    }
}

pub(crate) fn is_tidal_app_host(url: &str) -> bool {
    matches!(
        PageKind::classify(url),
        PageKind::DesktopApp | PageKind::LoginPage | PageKind::LoginCallback | PageKind::AuthHost
    )
}

pub(crate) fn needs_token_injection(url: &str) -> bool {
    let Ok(parsed) = Url::parse(url) else {
        return false;
    };
    let host = parsed.host_str().unwrap_or("");
    host == HOST_API || host == HOST_DESKTOP
}

pub(crate) fn is_token_endpoint(url: &str) -> bool {
    let Ok(parsed) = Url::parse(url) else {
        return false;
    };
    parsed.path().contains(PATH_OAUTH_TOKEN)
}
