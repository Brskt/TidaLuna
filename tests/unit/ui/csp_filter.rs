//! Tests for `src/ui/csp_filter.rs`, attached to it by `#[path]`.

use cef::ResourceType;

use super::is_document_url;

fn url(s: &str) -> crate::ui::nav::RequestUrl {
    crate::ui::nav::RequestUrl::new(s.to_string())
}

/// `None` is the service-worker precache's view, with no type to read: the path decides. A cold
/// start on a deep link has no extension; only CEF's type names the document, but the host
/// check outranks it: the login host's popup is a top-level document whose CSP is never stripped.
#[test]
fn a_document_is_the_shell_by_path_or_a_main_frame_load_on_the_app_host() {
    for (target, resource_type, expected) in [
        ("https://desktop.tidal.com/", None, true),
        ("https://desktop.tidal.com/index.html", None, true),
        ("https://desktop.tidal.com/lastfmcallback.html", None, true),
        ("https://desktop.tidal.com/assets/index-abc.js", None, false),
        ("https://desktop.tidal.com/assets/x.css", None, false),
        (
            "https://resources.tidal.com/images/x/80x80.jpg",
            None,
            false,
        ),
        ("https://api.tidal.com/v1/tracks/1", None, false),
        ("https://desktop.tidal.com/album/1", None, false),
        (
            "https://desktop.tidal.com/album/1",
            Some(ResourceType::MAIN_FRAME),
            true,
        ),
        (
            "https://login.tidal.com/authorize",
            Some(ResourceType::MAIN_FRAME),
            false,
        ),
    ] {
        let typed = resource_type.is_some();
        assert_eq!(
            is_document_url(&url(target), resource_type),
            expected,
            "{target}, typed: {typed}"
        );
    }
}
