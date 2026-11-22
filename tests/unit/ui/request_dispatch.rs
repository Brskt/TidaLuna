//! Tests for `src/ui/request_dispatch.rs`, attached to it by `#[path]`.

use super::*;

const DESKTOP: Option<&str> = Some("https://desktop.tidal.com");
const LOGIN: Option<&str> = Some("https://login.tidal.com");

enum Kind {
    MainFrame,
    SubFrame,
    Request(Option<&'static str>),
}

fn judge(surface: Surface, method: &str, raw: &str, kind: Kind) -> Dispatch {
    let url = RequestUrl::new(raw.to_string());
    let (is_navigation, main_frame, initiator) = match kind {
        Kind::MainFrame => (true, true, Some("null")),
        Kind::SubFrame => (true, false, Some("null")),
        Kind::Request(initiator) => (false, false, initiator),
    };
    Dispatch::of(
        surface,
        &Inbound {
            method,
            url: &url,
            is_navigation,
            main_frame,
            initiator,
        },
    )
}

/// Every request to a sign-in host meets one judgement: a malformed or unparseable URL is refused,
/// a navigation passes only as a main-frame GET, and any other request reaches only its exact route
/// from a TIDAL page. The sign-in page's own requests keep their query (`/api/email?...`).
#[test]
fn a_sign_in_host_request_passes_only_as_a_main_frame_get_or_its_exact_route() {
    use DispatchRefusal::*;
    use Kind::{MainFrame, Request, SubFrame};
    use Surface::{App, Dialog};
    let route = |route, asker| Dispatch::Route(AuthRequest { route, asker });
    let token = "https://auth.tidal.com/v1/oauth2/token";
    let authorize = "https://login.tidal.com/authorize";
    for (surface, method, raw, kind, expected) in [
        (
            App,
            "GET",
            "not a url",
            Request(DESKTOP),
            Dispatch::Refuse(UnparseableUrl),
        ),
        (
            App,
            "GET",
            "https://api.tidal.com/v1/oauth2/token",
            Request(DESKTOP),
            Dispatch::Pass,
        ),
        (
            App,
            "GET",
            "https://login.tidal.com./authorize",
            MainFrame,
            Dispatch::Refuse(MalformedHost),
        ),
        (
            Dialog,
            "GET",
            authorize,
            MainFrame,
            Dispatch::Refuse(DialogSurface),
        ),
        (
            Dialog,
            "GET",
            "https://api.tidal.com/v1/tracks/1",
            Request(None),
            Dispatch::Pass,
        ),
        (App, "GET", authorize, MainFrame, Dispatch::Pass),
        (
            App,
            "POST",
            authorize,
            MainFrame,
            Dispatch::Refuse(NavigationNotMainFrameGet),
        ),
        (
            App,
            "GET",
            authorize,
            SubFrame,
            Dispatch::Refuse(NavigationNotMainFrameGet),
        ),
        (
            App,
            "POST",
            token,
            Request(DESKTOP),
            route(AuthRoute::TokenGrant, Asker::Tidal),
        ),
        (
            App,
            "POST",
            token,
            Request(LOGIN),
            route(AuthRoute::TokenGrant, Asker::LoginHost),
        ),
        (
            App,
            "POST",
            token,
            Request(Some("null")),
            Dispatch::Refuse(ForeignAsker),
        ),
        (
            App,
            "GET",
            "https://login.tidal.com/api/email?e=x",
            Request(LOGIN),
            route(AuthRoute::LoginPage, Asker::LoginHost),
        ),
        (
            App,
            "POST",
            "https://login.tidal.com/oauth2/token",
            Request(DESKTOP),
            Dispatch::Refuse(NotARoute),
        ),
    ] {
        assert_eq!(
            judge(surface, method, raw, kind),
            expected,
            "{surface:?} {method} {raw}"
        );
    }
}
