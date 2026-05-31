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
