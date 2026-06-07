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
pub(crate) fn redact_url_query(url: &str) -> String {
    match url.find(['?', '#']) {
        Some(idx) => format!("{}?<redacted>", &url[..idx]),
        None => url.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_url_query_strips_query() {
        assert_eq!(
            redact_url_query("https://desktop.tidal.com/login/auth?code=abc123&state=xyz"),
            "https://desktop.tidal.com/login/auth?<redacted>"
        );
    }

    #[test]
    fn redact_url_query_strips_fragment() {
        assert_eq!(
            redact_url_query("https://desktop.tidal.com/cb#access_token=abc123"),
            "https://desktop.tidal.com/cb?<redacted>"
        );
    }

    #[test]
    fn redact_url_query_passthrough_without_query() {
        assert_eq!(
            redact_url_query("https://desktop.tidal.com/browse"),
            "https://desktop.tidal.com/browse"
        );
    }
}
