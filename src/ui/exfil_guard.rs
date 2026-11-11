use cef::*;

use crate::ui::nav::RequestUrl;

// Unconditionally cancels the request. Used by the exfiltration guard
// to block sendBeacon (RT_PING) to non-Tidal domains.
wrap_resource_request_handler! {
    pub(super) struct ExfilBlockHandler;

    impl ResourceRequestHandler {
        fn on_before_resource_load(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _request: Option<&mut Request>,
            _callback: Option<&mut Callback>,
        ) -> ReturnValue {
            ReturnValue::CANCEL
        }
    }
}

/// Wired into the browser, context and dialog dispatches alike, mirroring `store_proxy::intercept`.
/// Every sink funnels here as one `RT_IMAGE` (`src`, `setAttribute`, `srcset`, `<picture>`, a CSS
/// background, React's commit path), and a redirect hop re-enters the callback, covering an open
/// redirect too. Never gated: a host dropped from the list would surface only as a missing image.
pub(super) fn block_disallowed_image(
    resource_type: ResourceType,
    url: &RequestUrl,
    surface: &str,
    initiator: Option<&CefString>,
) -> Option<ResourceRequestHandler> {
    match image_verdict(resource_type, url, initiator) {
        ImageVerdict::NotAnImage | ImageVerdict::Allowed => None,
        ImageVerdict::Refused => {
            crate::verr!(
                "[EXFIL]  BLOCKED {surface} image to {}",
                crate::util::refused_host(url.as_str())
            );
            Some(ExfilBlockHandler::new())
        }
    }
}

/// An internal browser surface fetching from its own origin. DevTools serves its bundled
/// iconography from `devtools://devtools/...`, and the inspector is not the only such surface:
/// CEF builds Picture-in-Picture the same way, on the opener's request context, so neither can
/// be given a context of its own and both arrive here.
///
/// Both halves are load-bearing. The scheme test excludes web content outright: Chromium fills
/// the initiator from the initiating document, and a page cannot claim a non-web origin. The
/// origin match then stops an open inspector from reaching a host it was merely steered at, so
/// the plugin-supplied avatar the trust dialog loads over https stays judged by the allowlist.
fn is_internal_self_load(url: &RequestUrl, initiator: Option<&CefString>) -> bool {
    let Some(parsed) = url.parsed() else {
        return false;
    };
    let scheme = parsed.scheme();
    if scheme == "http" || scheme == "https" {
        return false;
    }
    let Some(host) = parsed.host_str() else {
        return false;
    };
    initiator.is_some_and(|i| i.to_string() == format!("{scheme}://{host}"))
}

/// What the allowlist says about one request. Three outcomes, so a named enum: a bool would fold
/// `NotAnImage` into `Allowed` and lose the distinction that matters here; the gate judges where an
/// image comes from, and it judges that for images alone. Matched exhaustively above: a fourth
/// outcome cannot be added without deciding what the handler does with it.
#[derive(Debug, PartialEq, Eq)]
enum ImageVerdict {
    NotAnImage,
    Allowed,
    Refused,
}

/// The rule, kept free of IO. The refusal it feeds ends in an ungated `verr!`, which opens the
/// persistent log sink on first use; a test that called the handler would write to the real
/// `console.log`; the decision has to be reachable on its own to be exercised at all.
fn image_verdict(
    resource_type: ResourceType,
    url: &RequestUrl,
    initiator: Option<&CefString>,
) -> ImageVerdict {
    if resource_type != ResourceType::IMAGE {
        return ImageVerdict::NotAnImage;
    }
    if is_internal_self_load(url, initiator) || crate::ui::nav::is_secure_image_target(url) {
        ImageVerdict::Allowed
    } else {
        ImageVerdict::Refused
    }
}

#[cfg(test)]
#[path = "../../tests/unit/ui/exfil_guard.rs"]
mod tests;
