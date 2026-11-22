//! The judgement every request to TIDAL's sign-in hosts meets first, in each CEF handler that sees
//! requests: the app's, the browser-less context's and the dialogs'. The first handler that answers
//! wins; this one runs ahead of anything else they serve.

use cef::*;

use crate::ui::hosts::HostVerdict;
use crate::ui::nav::{Asker, AuthRoute, RequestUrl};

/// A request let through to a sign-in route, and who asked for it: read once, when CEF asks for
/// the handler of this hop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AuthRequest {
    pub(crate) route: AuthRoute,
    pub(crate) asker: Asker,
}

/// What a request is for the sign-in hosts, judged without IO.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Dispatch {
    /// Not a sign-in host, or the sign-in page loading in the main frame: the handler's own chain.
    Pass,
    Refuse(DispatchRefusal),
    Route(AuthRequest),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DispatchRefusal {
    UnparseableUrl,
    MalformedHost,
    ForeignAsker,
    NotARoute,
    NavigationNotMainFrameGet,
    DialogSurface,
}

impl std::fmt::Display for DispatchRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::UnparseableUrl => "its url does not parse",
            Self::MalformedHost => "its TIDAL host is malformed",
            Self::ForeignAsker => "no TIDAL page asked for it",
            Self::NotARoute => "it is no sign-in route",
            Self::NavigationNotMainFrameGet => "a sign-in host is opened only by a main-frame GET",
            Self::DialogSurface => "a dialog never reaches a sign-in host",
        })
    }
}

/// The handler that judges the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Surface {
    App,
    Dialog,
}

/// What CEF tells a handler about a request, before any of it is acted on.
pub(crate) struct Inbound<'a> {
    pub(crate) method: &'a str,
    pub(crate) url: &'a RequestUrl,
    pub(crate) is_navigation: bool,
    /// Read on the request's type, never on its frame, which falls back to the main frame.
    pub(crate) main_frame: bool,
    pub(crate) initiator: Option<&'a str>,
}

impl Dispatch {
    /// A navigation carries no initiator worth reading (`"null"`). It is judged on its type and
    /// method alone; any other request on who asked and its exact route.
    pub(crate) fn of(surface: Surface, req: &Inbound) -> Self {
        let Some(url) = req.url.parsed() else {
            return Self::Refuse(DispatchRefusal::UnparseableUrl);
        };
        match HostVerdict::of(url) {
            HostVerdict::Open => return Self::Pass,
            HostVerdict::MalformedTidal => return Self::Refuse(DispatchRefusal::MalformedHost),
            HostVerdict::SignIn => {}
        }
        if surface == Surface::Dialog {
            return Self::Refuse(DispatchRefusal::DialogSurface);
        }
        if req.is_navigation {
            return if req.main_frame && req.method == "GET" {
                Self::Pass
            } else {
                Self::Refuse(DispatchRefusal::NavigationNotMainFrameGet)
            };
        }
        let asker = Asker::of_initiator(req.initiator);
        if asker == Asker::Foreign {
            return Self::Refuse(DispatchRefusal::ForeignAsker);
        }
        match AuthRoute::of(req.method, url, asker) {
            AuthRoute::NotAuth | AuthRoute::Refused => Self::Refuse(DispatchRefusal::NotARoute),
            route => Self::Route(AuthRequest { route, asker }),
        }
    }
}

/// The handler a refused request gets: cancelled, with its host and the reason, never its URL.
pub(crate) fn refuse(
    surface: &str,
    why: DispatchRefusal,
    url: &RequestUrl,
) -> ResourceRequestHandler {
    crate::verr!(
        "[AUTH] {surface} request to {} refused: {why}",
        crate::util::refused_host(url.as_str())
    );
    crate::ui::exfil_guard::ExfilBlockHandler::new()
}

#[cfg(test)]
#[path = "../../tests/unit/ui/request_dispatch.rs"]
mod tests;
