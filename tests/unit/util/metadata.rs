//! Tests for `src/util/metadata.rs`, attached to it by `#[path]`.

use super::{parse_track_metadata, trimmed_non_empty};
use serde_json::json;

#[test]
fn a_string_that_is_present_in_name_only_is_absent() {
    // Every ingress that mints a track id funnels through here, because `same_track` calls
    // two equal ids one track: a pair of blanks would name one another. The Connect wire
    // field is a required `String` with no floor of its own, which is how one arrives.
    assert_eq!(trimmed_non_empty(""), None);
    assert_eq!(trimmed_non_empty("   "), None);
    assert_eq!(trimmed_non_empty("\t\n "), None);
}

#[test]
fn a_string_keeps_its_value_without_its_padding() {
    // Trimmed rather than merely accepted: two spellings of one id are one track.
    assert_eq!(trimmed_non_empty(" 88264189 ").as_deref(), Some("88264189"));
    assert_eq!(trimmed_non_empty("88264189").as_deref(), Some("88264189"));
}

#[test]
fn reads_the_numeric_id_tidal_sends() {
    let meta = parse_track_metadata(&json!({ "title": "Automatic", "id": 88264189 }));
    assert_eq!(meta.id.as_deref(), Some("88264189"));
    assert_eq!(meta.title, "Automatic");
}

#[test]
fn reads_the_string_id_plugins_send() {
    let meta = parse_track_metadata(&json!({ "title": "Shona", "id": " 120002099 " }));
    assert_eq!(meta.id.as_deref(), Some("120002099"));
}

#[test]
fn reads_the_product_id_the_dash_auto_advance_sends() {
    // `frontend/src/index.ts` announces the next DASH track as `{ productId, type }` with
    // no `id` at all. Missing it left every such frame unidentified, and an unidentified
    // frame can never match: the length was withheld for the whole track.
    let meta = parse_track_metadata(&json!({ "productId": 88264189, "type": "track" }));
    assert_eq!(meta.id.as_deref(), Some("88264189"));
}

#[test]
fn product_id_is_read_before_id() {
    // The frontend resolves identity as `item.productId ?? item.id`; disagreeing on which
    // key wins would make the two sides call the same payload two different tracks.
    let meta = parse_track_metadata(&json!({ "productId": "1", "id": "2" }));
    assert_eq!(meta.id.as_deref(), Some("1"));
}

#[test]
fn an_absent_or_blank_id_is_none() {
    // None must stay None rather than becoming an empty string: callers use the id to
    // tell tracks apart, and two unidentified tracks are not the same track.
    assert_eq!(parse_track_metadata(&json!({ "title": "X" })).id, None);
    assert_eq!(parse_track_metadata(&json!({ "id": "   " })).id, None);
    assert_eq!(parse_track_metadata(&json!({ "id": null })).id, None);
}

#[test]
fn the_other_fields_survive_the_new_one() {
    let meta = parse_track_metadata(&json!({
        "title": "  Automatic  ",
        "artists": [{ "name": "John Murphy" }, { "name": "Guest" }],
        "audioQuality": "LOSSLESS",
        "id": 1,
    }));
    assert_eq!(meta.title, "Automatic");
    assert_eq!(meta.artist, "John Murphy, Guest");
    assert_eq!(meta.quality, "LOSSLESS");
}
