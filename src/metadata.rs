use serde_json::Value;

fn value_trimmed_string(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
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

pub(crate) fn parse_track_metadata(payload: &Value) -> crate::state::TrackMetadata {
    let title = first_trimmed_string(payload, &["title", "name"]).unwrap_or_default();
    let quality = first_trimmed_string(payload, &["audioQuality", "quality"]).unwrap_or_default();
    let artist = parse_media_item_artist(payload);

    crate::state::TrackMetadata {
        title,
        artist,
        quality,
    }
}
