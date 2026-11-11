use crate::state::{CURRENT_METADATA, CURRENT_TRACK};

pub(crate) fn canonical_track_id(url: &str) -> String {
    url.split('?').next().unwrap_or(url).to_string()
}

/// Refreshes the retained credential only if the retained track is still the one
/// `canonical_url_id` names. Not redundant with the caller's guard, which reads
/// `committed_track`, set once a load reaches the pipeline, where this record is written
/// synchronously: the two legitimately disagree mid-load, and a duplicate `load(A)` racing
/// behind `load(B)` would stamp A's credential onto B's still-running download, killing it
/// silently at its next reconnect. A write is authorised by the state it writes, never by
/// another that lags it. `load_gen` is left alone: re-signing is not a new load.
pub(crate) fn refresh_retained_credential(
    retained: Option<&mut crate::state::RetainedTrack>,
    canonical_url_id: &str,
    url: &str,
    key: &str,
    product_id: Option<&str>,
) {
    let Some(track) = retained
        .map(|retained| &mut retained.track)
        .filter(|track| canonical_track_id(&track.url) == canonical_url_id)
    else {
        return;
    };
    track.url = url.to_string();
    track.key = key.to_string();
    // Refreshes what this load knows and erases nothing it does not: a load arriving without an
    // id must not blank the one a later replay of this same source still needs.
    if let Some(id) = product_id {
        track.product_id = Some(id.to_string());
    }
}

/// The id retained for a source, when the retained source is this one. A recover is handed a
/// fresh credential for the track already playing and no identity at all; the retained copy is
/// where that identity was last known.
pub(super) fn retained_product_id(canonical_url_id: &str) -> Option<String> {
    CURRENT_TRACK
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retained()
        .map(|retained| &retained.track)
        .filter(|track| canonical_track_id(&track.url) == canonical_url_id)
        .and_then(|track| track.product_id.clone())
}

/// The url a running download must fetch with, for the track it started for. The signed url is a
/// credential, not identity: the SDK re-signs it every load and re-resolves it once its 1h expiry
/// lapses, so a captured copy goes stale while the task legitimately runs on, and reconnects and
/// range restarts read through here instead. `None` means the task's track was replaced and it
/// must stop, making explicit an invariant `load_with_policy` already keeps.
pub(crate) fn refreshed_fetch_url(
    task_canonical_id: &str,
    retained: Option<&crate::state::TrackInfo>,
) -> Option<String> {
    retained
        .filter(|track| canonical_track_id(&track.url) == task_canonical_id)
        .map(|track| track.url.clone())
}

/// True when a `load` targets the already-committed track: same canonical id
/// (query stripped, since each load re-signs the CDN URL) and format. Re-loading
/// would rebuild an identical pipeline: the caller resumes instead. Both ids
/// are canonical (caller strips the URL; `committed.0` is set at commit time).
pub(super) fn is_same_active_track(
    committed: Option<&(String, String)>,
    canonical_url_id: &str,
    format: &str,
) -> bool {
    committed.is_some_and(|(id, fmt)| fmt == format && id == canonical_url_id)
}

/// Whether the announced metadata describes a track other than this load's. The slot answers
/// "what was announced last", never "which track is this load", and a Rust-driven advance runs
/// ahead of the announcement. Only a POSITIVE disagreement counts: a load carrying no id of its
/// own (what a recover through `retained_product_id` looks like) is no evidence of staleness,
/// and treating it as such would withhold a title that was correct.
pub(crate) fn announcement_is_stale(load: Option<&str>, announced: Option<&str>) -> bool {
    matches!((load, announced), (Some(a), Some(b)) if a != b)
}

/// `product_id` is the load's own id, used only to tell whether the announced metadata
/// describes this load. Logging-only: nothing downstream reads the banner.
pub(super) fn print_track_banner(format: &str, product_id: Option<&str>) {
    let Ok(lock) = CURRENT_METADATA.lock() else {
        crate::vprintln!("[PLAYER] CURRENT_METADATA lock poisoned, skipping banner");
        return;
    };
    let format_upper = format.to_uppercase();
    let announced_id = lock.as_ref().and_then(|m| m.id.as_deref());
    let unannounced = product_id.filter(|load| announcement_is_stale(Some(load), announced_id));
    let (title, artist, quality) = match (unannounced, lock.as_ref()) {
        (Some(id), _) => (id, "not announced yet", format_upper.as_str()),
        (None, Some(m)) => (
            if m.title.is_empty() {
                "Unknown"
            } else {
                m.title.as_str()
            },
            if m.artist.is_empty() {
                "Unknown"
            } else {
                m.artist.as_str()
            },
            if m.quality.is_empty() {
                format_upper.as_str()
            } else {
                m.quality.as_str()
            },
        ),
        (None, None) => ("Unknown", "Unknown", format_upper.as_str()),
    };
    crate::vprintln!("══════════════════════════════════════════");
    crate::vprintln!("  {} - {}", title, artist);
    crate::vprintln!("  Quality: {} | Format: {}", quality, format);
    crate::vprintln!("══════════════════════════════════════════");
}

#[cfg(test)]
#[path = "../../tests/unit/player/reconcile_tests.rs"]
mod reconcile_tests;

#[cfg(test)]
#[path = "../../tests/unit/player/fetch_url_tests.rs"]
mod fetch_url_tests;

#[cfg(test)]
#[path = "../../tests/unit/player/banner_tests.rs"]
mod banner_tests;
