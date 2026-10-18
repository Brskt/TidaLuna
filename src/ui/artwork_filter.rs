//! Drop TIDAL's video album covers from JSON:API responses.
//!
//! The CEF binary we ship carries no H.264 decoder; Chromium's codec allowlist
//! refuses the `.mp4` artwork before a decoder is ever picked, and TIDAL answers
//! that failure with a near-blank placeholder rather than the cover it owns.
//!
//! Both artworks travel in the same compound document: `included` carries an
//! `IMAGE` resource next to the `VIDEO` one, and TIDAL already holds a selector
//! for each. Removing the video resource hands the choice back to code TIDAL
//! wrote, on data it sent, at every surface that shares the component rather
//! than at the one screen where the defect was noticed.

use serde_json::Value;

use crate::ui::nav::RequestUrl;

/// The JSON:API gateway that carries `artworks` resources. TIDAL's legacy hosts
/// describe covers with flat `cover`/`videoCover` fields instead. That shape needs
/// its own transform, and this module leaves it alone.
const HOST_OPENAPI: &str = "openapi.tidal.com";

/// Asked on its own by the request half of the contract: compression has to be
/// refused before the response exists, when its mime type is still unknown.
pub(crate) fn serves_artworks(url: &RequestUrl) -> bool {
    url.parsed()
        .is_some_and(|parsed| parsed.scheme() == "https" && parsed.host_str() == Some(HOST_OPENAPI))
}

/// Keeps the buffering filter off everything on the host that is not a JSON
/// document. Its own half of the rule, because the two are asked at different
/// moments. The CEF path knows the host before the response exists and the mime
/// only once it does.
pub(crate) fn carries_json(mime: &str) -> bool {
    mime.contains("json")
}

pub(crate) fn should_strip(url: &RequestUrl, mime: &str) -> bool {
    carries_json(mime) && serves_artworks(url)
}

/// The request half, with the decoder's readiness folded in. Readiness, not the switch: a switch
/// left on over a codec that failed to download would hand TIDAL a video nothing can paint.
/// A parameter, never read here: this module's rules run under tests that hold no process state.
pub(crate) fn arms_artwork_filter(url: &RequestUrl, video_codec_ready: bool) -> bool {
    !video_codec_ready && serves_artworks(url)
}

/// The body half, for the path that fetches upstream itself and so has no CEF response to
/// attach a filter to.
pub(crate) fn strips_artwork_body(url: &RequestUrl, mime: &str, video_codec_ready: bool) -> bool {
    !video_codec_ready && should_strip(url, mime)
}

pub(crate) enum StripResult {
    Rewritten(Vec<u8>),
    /// Nothing to do: the caller emits the body it already holds. Unlike a token
    /// payload, passing this one through unchanged leaks nothing.
    Unchanged,
}

/// The identifiers naming a removed artwork go with it, not just the resource.
pub(crate) fn strip_video_artwork(body: &[u8]) -> StripResult {
    let Ok(mut doc) = serde_json::from_slice::<Value>(body) else {
        // The mime said JSON. Worth a line rather than a silent skip: the
        // rewrite does not happen for this response.
        crate::vprintln2!("[artwork_filter] response on {HOST_OPENAPI} is not valid JSON");
        return StripResult::Unchanged;
    };

    let removed = take_video_artworks(&mut doc);
    if removed.is_empty() {
        return StripResult::Unchanged;
    }
    prune_references(&mut doc, &removed);

    match serde_json::to_vec(&doc) {
        Ok(bytes) => StripResult::Rewritten(bytes),
        Err(e) => {
            crate::verr!("[artwork_filter] re-serializing the stripped document failed: {e}");
            StripResult::Unchanged
        }
    }
}

fn take_video_artworks(doc: &mut Value) -> Vec<String> {
    let Some(included) = doc.get_mut("included").and_then(Value::as_array_mut) else {
        return Vec::new();
    };

    let mut removed = Vec::new();
    included.retain(|resource| {
        if !is_video_artwork(resource) {
            return true;
        }
        if let Some(id) = resource.get("id").and_then(Value::as_str) {
            removed.push(id.to_owned());
        }
        false
    });
    removed
}

fn is_video_artwork(resource: &Value) -> bool {
    resource.get("type").and_then(Value::as_str) == Some("artworks")
        && resource
            .pointer("/attributes/mediaType")
            .and_then(Value::as_str)
            == Some("VIDEO")
}

/// Pruning is not cosmetic: several TIDAL selectors read `coverArt.data[0]`, and a
/// hole left at index 0 blanks a cover the remaining static artwork can fill.
///
/// Only identifiers under a `relationships` map are touched: a top-level `data` is
/// the resource the caller asked for, and emptying it would answer a request for
/// an artwork with nothing at all.
fn prune_references(node: &mut Value, removed: &[String]) {
    match node {
        Value::Array(items) => {
            for item in items {
                prune_references(item, removed);
            }
        }
        Value::Object(map) => {
            if let Some(relationships) = map.get_mut("relationships").and_then(Value::as_object_mut)
            {
                for relationship in relationships.values_mut() {
                    prune_identifiers(relationship, removed);
                }
            }
            for value in map.values_mut() {
                prune_references(value, removed);
            }
        }
        _ => {}
    }
}

/// JSON:API allows a relationship's `data` to be either a list of identifiers or a
/// single one, hence the two arms.
fn prune_identifiers(relationship: &mut Value, removed: &[String]) {
    let Some(data) = relationship.get_mut("data") else {
        return;
    };
    match data {
        Value::Array(identifiers) => {
            identifiers.retain(|identifier| !names_removed_artwork(identifier, removed));
        }
        single if names_removed_artwork(single, removed) => *single = Value::Null,
        _ => {}
    }
}

fn names_removed_artwork(identifier: &Value, removed: &[String]) -> bool {
    identifier.get("type").and_then(Value::as_str) == Some("artworks")
        && identifier
            .get("id")
            .and_then(Value::as_str)
            .is_some_and(|id| removed.iter().any(|gone| gone == id))
}

#[cfg(test)]
#[path = "../../tests/unit/ui/artwork_filter.rs"]
mod tests;
