use std::borrow::Cow;

pub(crate) mod fmt;
pub(crate) mod metadata;

/// Truncate `s` to at most `max_bytes`, snapping the end down to a UTF-8 char
/// boundary; it never panics on multi-byte characters.
pub(crate) fn truncate_str(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Replace a URL's query string and/or fragment with a redaction marker so
/// secrets carried there (OAuth `code`/`state`, tokens in a fragment) never reach
/// logs. The scheme/host/path are kept for debugging.
pub(crate) fn redact_url_query(url: &str) -> Cow<'_, str> {
    match url.find(['?', '#']) {
        Some(idx) => Cow::Owned(format!("{}?<redacted>", &url[..idx])),
        // No query/fragment: borrow the input, no allocation (the common case).
        None => Cow::Borrowed(url),
    }
}

/// Long enough for any real CDN hostname; the cap exists because the caller picks the length.
const REFUSED_HOST_MAX: usize = 64;

/// Bounded host only, never the full url: it reaches a refusal log from an untrusted source, and
/// `verr!` is not gated. Logging it verbatim would let a caller write unbounded text, escapes
/// intact, into the persistent log; a query would carry its credential there too. The host is what
/// makes an unexpected regional CDN reportable.
pub(crate) fn refused_host(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(parsed) => {
            truncate_str(parsed.host_str().unwrap_or("no host"), REFUSED_HOST_MAX).to_string()
        }
        Err(_) => "unparseable url".to_string(),
    }
}

/// A network failure's text with the request URL removed. `reqwest::Error`'s `Display` appends the
/// URL with its query, and a media URL's query is a time-bounded CDN credential. This text does not
/// stay in a log: `MediaError` carries it into the renderer's JS realm, into TIDAL's telemetry, and
/// out over the Connect socket to every controller on the network. Every one of those sinks
/// branches off the same `String`; the URL has to go here, at its origin.
pub(crate) fn network_error_text(e: reqwest::Error) -> String {
    e.without_url().to_string()
}

/// True for a package-manager install (sets `TIDALUNAR_MANAGED_INSTALL`): read-only
/// and self-managed. Skip the desktop self-install and the in-app updater.
pub(crate) fn is_managed_install() -> bool {
    managed_install_from(std::env::var("TIDALUNAR_MANAGED_INSTALL").ok().as_deref())
}

fn managed_install_from(value: Option<&str>) -> bool {
    matches!(value, Some(v) if !v.is_empty() && v != "0")
}

#[cfg(test)]
#[path = "../../tests/unit/util.rs"]
mod tests;
