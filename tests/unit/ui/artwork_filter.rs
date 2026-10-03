//! Tests for `src/ui/artwork_filter.rs`, attached to it by `#[path]`.

use super::*;

/// A compound document shaped like the album payload TIDAL answers with: the
/// video artwork and the static one sit side by side, video first, exactly the
/// order that puts the unplayable resource at `coverArt.data[0]`.
fn album_with_both_artworks() -> String {
    serde_json::json!({
        "data": {
            "type": "albums",
            "id": "175122451",
            "relationships": {
                "coverArt": {
                    "data": [
                        { "type": "artworks", "id": "vid-1" },
                        { "type": "artworks", "id": "img-1" }
                    ]
                }
            }
        },
        "included": [
            {
                "type": "artworks",
                "id": "vid-1",
                "attributes": {
                    "mediaType": "VIDEO",
                    "files": [{ "href": "https://resources.tidal.com/videos/a/1280x1280.mp4",
                                "meta": { "width": 1280, "height": 1280 } }]
                }
            },
            {
                "type": "artworks",
                "id": "img-1",
                "attributes": {
                    "mediaType": "IMAGE",
                    "files": [{ "href": "https://resources.tidal.com/images/a/1280x1280.jpg",
                                "meta": { "width": 1280, "height": 1280 } }]
                }
            }
        ]
    })
    .to_string()
}

fn rewritten(body: &str) -> Value {
    match strip_video_artwork(body.as_bytes()) {
        StripResult::Rewritten(bytes) => serde_json::from_slice(&bytes).expect("valid JSON out"),
        StripResult::Unchanged => panic!("expected the document to be rewritten"),
    }
}

#[test]
fn drops_the_video_artwork_and_keeps_the_static_one() {
    let doc = rewritten(&album_with_both_artworks());

    let included = doc["included"].as_array().expect("included survives");
    assert_eq!(included.len(), 1);
    assert_eq!(included[0]["id"], "img-1");
    assert_eq!(included[0]["attributes"]["mediaType"], "IMAGE");
}

#[test]
fn prunes_the_identifier_so_the_static_artwork_lands_first() {
    let doc = rewritten(&album_with_both_artworks());

    // TIDAL selectors read coverArt.data[0]; a hole left there blanks the cover
    // just as surely as the unplayable video did.
    let ids = doc["data"]["relationships"]["coverArt"]["data"]
        .as_array()
        .expect("the relationship survives");
    assert_eq!(ids.len(), 1);
    assert_eq!(ids[0]["id"], "img-1");
}

#[test]
fn leaves_a_document_without_video_artwork_alone() {
    let body = serde_json::json!({
        "data": { "type": "albums", "id": "1" },
        "included": [
            { "type": "artworks", "id": "img-1", "attributes": { "mediaType": "IMAGE" } }
        ]
    })
    .to_string();

    assert!(matches!(
        strip_video_artwork(body.as_bytes()),
        StripResult::Unchanged
    ));
}

#[test]
fn keeps_a_video_artwork_that_is_the_requested_resource() {
    // GET /artworks/{id} answers with the artwork itself in `data`. Emptying it
    // would answer the request with nothing rather than with a static cover.
    let body = serde_json::json!({
        "data": {
            "type": "artworks",
            "id": "vid-1",
            "attributes": { "mediaType": "VIDEO" }
        }
    })
    .to_string();

    assert!(matches!(
        strip_video_artwork(body.as_bytes()),
        StripResult::Unchanged
    ));
}

#[test]
fn nulls_a_to_one_relationship_naming_a_removed_artwork() {
    let body = serde_json::json!({
        "data": {
            "type": "albums",
            "id": "1",
            "relationships": { "coverArt": { "data": { "type": "artworks", "id": "vid-1" } } }
        },
        "included": [
            { "type": "artworks", "id": "vid-1", "attributes": { "mediaType": "VIDEO" } }
        ]
    })
    .to_string();

    let doc = rewritten(&body);
    assert!(doc["data"]["relationships"]["coverArt"]["data"].is_null());
}

#[test]
fn prunes_identifiers_carried_by_an_included_resource() {
    // A track's album can arrive under `included` too, carrying its own
    // coverArt identifiers: the walk has to reach them, not just the top level.
    let body = serde_json::json!({
        "data": { "type": "tracks", "id": "9" },
        "included": [
            {
                "type": "albums",
                "id": "1",
                "relationships": {
                    "coverArt": { "data": [{ "type": "artworks", "id": "vid-1" }] }
                }
            },
            { "type": "artworks", "id": "vid-1", "attributes": { "mediaType": "VIDEO" } }
        ]
    })
    .to_string();

    let doc = rewritten(&body);
    let album = doc["included"]
        .as_array()
        .and_then(|r| r.iter().find(|e| e["type"] == "albums"))
        .expect("the album survives");
    assert!(
        album["relationships"]["coverArt"]["data"]
            .as_array()
            .expect("the relationship survives")
            .is_empty()
    );
}

#[test]
fn leaves_an_unrelated_relationship_alone() {
    let body = serde_json::json!({
        "data": {
            "type": "albums",
            "id": "1",
            "relationships": {
                "coverArt": { "data": [{ "type": "artworks", "id": "vid-1" }] },
                "artists": { "data": [{ "type": "artists", "id": "vid-1" }] }
            }
        },
        "included": [
            { "type": "artworks", "id": "vid-1", "attributes": { "mediaType": "VIDEO" } }
        ]
    })
    .to_string();

    // Same id, different resource type: the artist identifier must survive.
    let doc = rewritten(&body);
    let artists = doc["data"]["relationships"]["artists"]["data"]
        .as_array()
        .expect("the relationship survives");
    assert_eq!(artists.len(), 1);
    assert_eq!(artists[0]["type"], "artists");
}

#[test]
fn passes_through_a_body_that_is_not_json() {
    assert!(matches!(
        strip_video_artwork(b"<html>not json</html>"),
        StripResult::Unchanged
    ));
}

#[test]
fn strips_only_json_on_the_openapi_host() {
    let openapi = RequestUrl::new("https://openapi.tidal.com/v2/albums/1".to_owned());
    assert!(should_strip(&openapi, "application/vnd.api+json"));
    assert!(!should_strip(&openapi, "text/html"));

    // The legacy gateway describes covers with flat fields; a different shape
    // needs its own transform.
    let legacy = RequestUrl::new("https://api.tidal.com/v1/albums/1".to_owned());
    assert!(!should_strip(&legacy, "application/json"));

    let plaintext = RequestUrl::new("http://openapi.tidal.com/v2/albums/1".to_owned());
    assert!(!should_strip(&plaintext, "application/json"));
}

#[test]
fn the_filter_arms_on_the_gateway_while_the_codec_is_off() {
    let url = RequestUrl::new("https://openapi.tidal.com/v2/albums/1".to_owned());
    assert!(arms_artwork_filter(&url, false));
}

#[test]
fn the_codec_switch_disarms_the_filter_on_the_same_url() {
    let url = RequestUrl::new("https://openapi.tidal.com/v2/albums/1".to_owned());
    assert!(!arms_artwork_filter(&url, true));
}

#[test]
fn the_switch_cannot_arm_the_filter_on_another_host() {
    let url = RequestUrl::new("https://listen.tidal.com/v2/albums/1".to_owned());
    assert!(!arms_artwork_filter(&url, false));
    assert!(!arms_artwork_filter(&url, true));
}

#[test]
fn the_body_rule_strips_json_on_the_gateway_while_the_codec_is_off() {
    let url = RequestUrl::new("https://openapi.tidal.com/v2/albums/1".to_owned());
    assert!(strips_artwork_body(&url, "application/vnd.api+json", false));
}

#[test]
fn the_codec_switch_lets_the_body_through_untouched() {
    let url = RequestUrl::new("https://openapi.tidal.com/v2/albums/1".to_owned());
    assert!(!strips_artwork_body(&url, "application/vnd.api+json", true));
}

#[test]
fn a_non_json_body_is_left_alone_whatever_the_switch_says() {
    let url = RequestUrl::new("https://openapi.tidal.com/v2/albums/1".to_owned());
    assert!(!strips_artwork_body(&url, "image/jpeg", false));
    assert!(!strips_artwork_body(&url, "image/jpeg", true));
}
