//! What a URL's host is, and whether a request Rust sends may go there.
//!
//! A host is judged on what `url` parsed, never on the raw string. A suffix test reads an empty
//! label or a final dot as a subdomain: `a..tidal.com` and `tidal.com.` cleared every suffix
//! gate, and Chromium resolves both. Such a host is never a [`Name`]. No suffix gate admits it.

use url::Url;

use crate::ui::nav::{HOST_AUTH, HOST_LOGIN};

/// The registrable domains TIDAL owns. No apex is a suffix of another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TidalDomain {
    Tidal,
    TidalHifi,
    TidalHi,
}

impl TidalDomain {
    const ALL: [Self; 3] = [Self::Tidal, Self::TidalHifi, Self::TidalHi];

    pub(crate) fn apex(self) -> &'static str {
        match self {
            Self::Tidal => "tidal.com",
            Self::TidalHifi => "tidalhifi.com",
            Self::TidalHi => "tidalhi.fi",
        }
    }

    fn of(host: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|domain| within(host, domain.apex()))
    }
}

/// `host` is `apex` or one of its subdomains, on a label boundary. Sound only on a [`Name`]: on a
/// raw host, `a..tidal.com` passes. The one family test, shared by [`Name`] and [`HostVerdict`].
fn within(host: &str, apex: &str) -> bool {
    host == apex
        || host
            .strip_suffix(apex)
            .is_some_and(|rest| rest.ends_with('.'))
}

/// A domain whose every label is non-empty: no final dot, no `..`, no leading dot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Name<'a>(&'a str);

impl<'a> Name<'a> {
    fn new(host: &'a str) -> Option<Self> {
        host.split('.')
            .all(|label| !label.is_empty())
            .then_some(Self(host))
    }

    pub(crate) fn is(self, host: &str) -> bool {
        self.0 == host
    }

    pub(crate) fn within(self, apex: &str) -> bool {
        within(self.0, apex)
    }

    pub(crate) fn first_label(self) -> &'a str {
        self.0.split('.').next().unwrap_or(self.0)
    }

    pub(crate) fn domain(self) -> Option<TidalDomain> {
        TidalDomain::of(self.0)
    }

    fn labels(self) -> impl Iterator<Item = &'a str> {
        self.0.split('.')
    }
}

/// The host of a parsed URL, read from `Url::host`: an IP literal is not a name, whatever its text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HostShape<'a> {
    Name(Name<'a>),
    /// A domain with an empty label or a final dot.
    Malformed(&'a str),
    /// An IP literal, or no host at all (`blob:`, `data:`, a non-special scheme).
    NotAName,
}

impl<'a> HostShape<'a> {
    pub(crate) fn of(url: &'a Url) -> Self {
        match url.host() {
            Some(url::Host::Domain(host)) => {
                Name::new(host).map_or(Self::Malformed(host), Self::Name)
            }
            _ => Self::NotAName,
        }
    }
}

/// Where a request Rust sends may go, by host alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HostVerdict {
    Open,
    /// A TIDAL host with an `auth` or `login` label anywhere, `auth.stage.tidal.com` included.
    SignIn,
    /// A TIDAL host with an empty label or a final dot: refused, never normalised.
    MalformedTidal,
}

impl HostVerdict {
    /// An IP literal is `Open`: it names no TIDAL host to protect. The sign-in labels come from
    /// `HOST_AUTH` and `HOST_LOGIN` (a new sign-in host there is refused here too).
    pub(crate) fn of(url: &Url) -> Self {
        match HostShape::of(url) {
            HostShape::Name(name) => {
                let signs_in = name.labels().any(|label| {
                    [HOST_AUTH, HOST_LOGIN]
                        .iter()
                        .any(|host| host.split('.').next() == Some(label))
                });
                if signs_in && name.domain().is_some() {
                    Self::SignIn
                } else {
                    Self::Open
                }
            }
            HostShape::Malformed(host) => {
                if TidalDomain::of(host.trim_end_matches('.')).is_some() {
                    Self::MalformedTidal
                } else {
                    Self::Open
                }
            }
            HostShape::NotAName => Self::Open,
        }
    }

    pub(crate) fn refusal(self) -> Option<EgressRefusal> {
        match self {
            Self::Open => None,
            Self::SignIn => Some(EgressRefusal::SignInHost),
            Self::MalformedTidal => Some(EgressRefusal::MalformedTidalHost),
        }
    }
}

/// Why Rust refused to send a request. The text is fixed: it never carries the URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EgressRefusal {
    SignInHost,
    MalformedTidalHost,
    Unparseable,
    /// A scheme other than `http` or `https`.
    NotHttp,
    /// A `Host` header, which names a destination the URL does not.
    ForgedHostHeader,
    /// A redirect hop left the hosts the request may reach.
    RedirectedToRefused,
}

impl std::fmt::Display for EgressRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::SignInHost => "the destination is a TIDAL sign-in host",
            Self::MalformedTidalHost => "the destination is a malformed TIDAL host",
            Self::Unparseable => "the url does not parse",
            Self::NotHttp => "the url is not http(s)",
            Self::ForgedHostHeader => "a Host header is not the plugin's to set",
            Self::RedirectedToRefused => "a redirect reached a host plugins may not reach",
        })
    }
}

impl std::error::Error for EgressRefusal {}

/// A URL whose host [`HostVerdict`] let through. The judged `Url` is the one handed to reqwest:
/// nothing re-parses a string between the verdict and the send.
#[derive(Debug)]
pub(crate) struct PermittedUrl(Url);

impl PermittedUrl {
    pub(crate) fn new(url: Url) -> Result<Self, EgressRefusal> {
        match HostVerdict::of(&url).refusal() {
            Some(refusal) => Err(refusal),
            None => Ok(Self(url)),
        }
    }

    pub(crate) fn as_url(&self) -> &Url {
        &self.0
    }

    pub(crate) fn into_url(self) -> Url {
        self.0
    }
}

#[cfg(test)]
#[path = "../../tests/unit/ui/hosts.rs"]
mod tests;
