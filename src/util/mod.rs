use std::borrow::Cow;

pub(crate) mod fmt;
pub(crate) mod metadata;

/// Truncate `s` to at most `max_bytes`, snapping the end down to a UTF-8 char
/// boundary so it never panics on multi-byte characters.
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

/// True for a package-manager install (sets `TIDALUNAR_MANAGED_INSTALL`): read-only
/// and self-managed, so skip the desktop self-install and the in-app updater.
pub(crate) fn is_managed_install() -> bool {
    managed_install_from(std::env::var("TIDALUNAR_MANAGED_INSTALL").ok().as_deref())
}

fn managed_install_from(value: Option<&str>) -> bool {
    matches!(value, Some(v) if !v.is_empty() && v != "0")
}

#[cfg(test)]
#[path = "../../tests/unit/util.rs"]
mod tests;
