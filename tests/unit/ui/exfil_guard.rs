//! Tests for `src/ui/exfil_guard.rs`, attached to it by `#[path]`.

use super::*;

/// The gate's rule, lifted off the handler so it can be exercised at all: the refusal branch ends
/// in an ungated `verr!`, and calling that from a test opens the real `console.log`.
#[test]
fn image_verdict_refuses_a_disallowed_image_and_admits_an_internal_surface_loading_itself() {
    for (what, resource_type, target, initiator, expected) in [
        (
            "an image on a foreign host",
            ResourceType::IMAGE,
            "https://evil.example/p.png",
            None,
            ImageVerdict::Refused,
        ),
        (
            "an image on an allowed host",
            ResourceType::IMAGE,
            "https://resources.tidal.com/p.png",
            None,
            ImageVerdict::Allowed,
        ),
        (
            "any other resource type passes untouched",
            ResourceType::MAIN_FRAME,
            "https://evil.example/p.png",
            None,
            ImageVerdict::NotAnImage,
        ),
        (
            "a plaintext image on an allowed host",
            ResourceType::IMAGE,
            "http://avatars.githubusercontent.com/u/1?v=4",
            None,
            ImageVerdict::Refused,
        ),
        (
            "an internal surface loading itself",
            ResourceType::IMAGE,
            "devtools://devtools/bundled/i.svg",
            Some(CefString::from("devtools://devtools")),
            ImageVerdict::Allowed,
        ),
        (
            "a foreign image beside an internal initiator",
            ResourceType::IMAGE,
            "https://evil.example/p.png",
            Some(CefString::from("devtools://devtools")),
            ImageVerdict::Refused,
        ),
        (
            "an internal url under a page initiator",
            ResourceType::IMAGE,
            "devtools://devtools/bundled/i.svg",
            Some(CefString::from("https://desktop.tidal.com")),
            ImageVerdict::Refused,
        ),
        (
            "an https initiator earns no self-load pass",
            ResourceType::IMAGE,
            "https://evil.example/a.png",
            Some(CefString::from("https://evil.example")),
            ImageVerdict::Refused,
        ),
    ] {
        let url = RequestUrl::new(target.to_string());
        assert_eq!(
            image_verdict(resource_type, &url, initiator.as_ref()),
            expected,
            "{what}"
        );
    }
}
