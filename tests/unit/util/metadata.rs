//! Tests for `src/util/metadata.rs`, attached to it by `#[path]`.

use super::{parse_track_metadata, trimmed_non_empty};
use serde_json::json;

/// Every ingress that mints a track id funnels through here, and `same_track` calls two equal ids
/// one track: a pair of blanks would name one another, and two spellings of one id must trim alike.
#[test]
fn a_string_is_absent_when_blank_and_kept_without_its_padding_otherwise() {
    for (raw, expected) in [
        ("", None),
        ("   ", None),
        ("\t\n ", None),
        (" 88264189 ", Some("88264189")),
        ("88264189", Some("88264189")),
    ] {
        assert_eq!(trimmed_non_empty(raw).as_deref(), expected, "{raw:?}");
    }
}

#[test]
fn reads_the_numeric_id_tidal_sends() {
    let meta = parse_track_metadata(&json!({ "title": "Automatic", "id": 88264189 }));
    assert_eq!(meta.id.as_deref(), Some("88264189"));
    assert_eq!(meta.title, "Automatic");
}

/// Identity is `productId ?? id`, as the frontend resolves it: disagreeing on the winner would
/// make the two sides call one payload two tracks. A missing or blank id stays `None`, since two
/// unidentified tracks are not the same track.
#[test]
fn a_track_is_identified_by_its_trimmed_product_id_else_its_id() {
    for (payload, expected) in [
        (
            json!({ "title": "Shona", "id": " 120002099 " }),
            Some("120002099"),
        ),
        (
            json!({ "productId": 88264189, "type": "track" }),
            Some("88264189"),
        ),
        (json!({ "productId": "1", "id": "2" }), Some("1")),
        (json!({ "title": "X" }), None),
        (json!({ "id": "   " }), None),
        (json!({ "id": null }), None),
    ] {
        assert_eq!(
            parse_track_metadata(&payload).id.as_deref(),
            expected,
            "{payload}"
        );
    }
}
