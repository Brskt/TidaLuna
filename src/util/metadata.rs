use serde_json::Value;

/// A string that is present in name only is absent. Every ingress that mints a track id
/// funnels through here: `same_track` calls two ids the same track when they are equal. A
/// pair of blanks would name one another and lend a length across tracks.
pub(crate) fn trimmed_non_empty(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

fn value_trimmed_string(value: &Value) -> Option<String> {
    value.as_str().and_then(trimmed_non_empty)
}

/// A track id as JSON carries it. TIDAL sends its own ids as numbers, everything else (the
/// plugins, the Connect payloads) as strings; both spellings have to read as one track.
/// Every ingress goes through here: a load and the metadata frame that must match it cannot
/// disagree on whether `88264189` and `"88264189"` name the same thing.
pub(crate) fn value_track_id(value: &Value) -> Option<String> {
    value_trimmed_string(value).or_else(|| value.as_i64().map(|n| n.to_string()))
}

fn first_trimmed_string(obj: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| obj.get(*key).and_then(value_trimmed_string))
}

fn parse_media_item_artist(obj: &Value) -> String {
    if let Some(artist) = obj.get("artist") {
        if let Some(name) = value_trimmed_string(artist) {
            return name;
        }
        if let Some(name) = artist.get("name").and_then(value_trimmed_string) {
            return name;
        }
    }

    if let Some(artists) = obj.get("artists").and_then(|v| v.as_array()) {
        let names: Vec<String> = artists
            .iter()
            .filter_map(|artist| {
                value_trimmed_string(artist)
                    .or_else(|| artist.get("name").and_then(value_trimmed_string))
            })
            .collect();
        if !names.is_empty() {
            return names.join(", ");
        }
    }

    String::new()
}

/// The track id off a metadata payload. `productId` is read first because that is the order the
/// frontend resolves identity in, and the DASH auto-advance frame carries that key and no `id`.
fn parse_track_id(obj: &Value) -> Option<String> {
    ["productId", "id"]
        .into_iter()
        .find_map(|key| obj.get(key).and_then(value_track_id))
}

pub(crate) fn parse_track_metadata(payload: &Value) -> crate::state::TrackMetadata {
    let title = first_trimmed_string(payload, &["title", "name"]).unwrap_or_default();
    let quality = first_trimmed_string(payload, &["audioQuality", "quality"]).unwrap_or_default();
    let artist = parse_media_item_artist(payload);
    let id = parse_track_id(payload);

    crate::state::TrackMetadata {
        title,
        artist,
        quality,
        id,
    }
}

#[cfg(test)]
#[path = "../../tests/unit/util/metadata.rs"]
mod tests;
