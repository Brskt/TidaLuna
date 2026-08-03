//! Tests for `refreshed_fetch_url` in `src/player/mod.rs`, attached to it by `#[path]`.

use super::{refresh_retained_credential, refreshed_fetch_url};
use crate::state::TrackInfo;

fn track(url: &str) -> TrackInfo {
    TrackInfo {
        url: url.to_string(),
        key: String::new(),
        format: "flac".to_string(),
    }
}

/// The whole point: the signed query is refreshed under a running download, and the reconnect must
/// use the new one. Keeping the captured copy is what 403'd a track dead after its signature lapsed.
#[test]
fn a_refreshed_signature_is_handed_to_the_running_download() {
    let retained = track("https://cdn.test/mediatracks/abc?sig=FRESH");

    assert_eq!(
        refreshed_fetch_url("https://cdn.test/mediatracks/abc", Some(&retained)).as_deref(),
        Some("https://cdn.test/mediatracks/abc?sig=FRESH")
    );
}

/// A task whose track is no longer the retained one is stale and must stop, not fetch. Without this
/// the reconnect would download a DIFFERENT track's audio into this track's buffer.
#[test]
fn a_task_for_another_track_gets_nothing() {
    let retained = track("https://cdn.test/mediatracks/other?sig=FRESH");

    assert!(refreshed_fetch_url("https://cdn.test/mediatracks/abc", Some(&retained)).is_none());
}

#[test]
fn no_retained_track_yields_nothing() {
    assert!(refreshed_fetch_url("https://cdn.test/mediatracks/abc", None).is_none());
}

/// The refresh takes the new signature for the track it was asked about.
#[test]
fn the_retained_credential_is_refreshed_for_its_own_track() {
    let mut retained = Some(track("https://cdn.test/mediatracks/abc?sig=OLD"));

    refresh_retained_credential(
        &mut retained,
        "https://cdn.test/mediatracks/abc",
        "https://cdn.test/mediatracks/abc?sig=NEW",
        "key",
    );

    let refreshed = retained.expect("still retained");
    assert_eq!(refreshed.url, "https://cdn.test/mediatracks/abc?sig=NEW");
    assert_eq!(refreshed.key, "key");
}

/// The caller's guard reads the COMMITTED track, which the player thread sets, while this record is
/// written synchronously: a duplicate load can match the old committed track while a newer one is
/// already retained. Stamping the old credential on would strand the newer track's running download
/// on a canonical mismatch, and it dies silently.
#[test]
fn a_stale_load_cannot_overwrite_a_newer_tracks_credential() {
    let mut retained = Some(track("https://cdn.test/mediatracks/newer?sig=B"));

    refresh_retained_credential(
        &mut retained,
        "https://cdn.test/mediatracks/older",
        "https://cdn.test/mediatracks/older?sig=A",
        "key-a",
    );

    let untouched = retained.expect("still retained");
    assert_eq!(
        untouched.url, "https://cdn.test/mediatracks/newer?sig=B",
        "the newer track's credential must survive"
    );
    assert!(untouched.key.is_empty(), "and its key too");
}

/// Identity is the query-stripped path; a task started on one signature still matches the track
/// after it is re-signed. That match is exactly what lets the refresh reach it.
#[test]
fn identity_ignores_the_signature_while_the_fetch_url_carries_it() {
    let retained = track("https://cdn.test/mediatracks/abc?sig=SECOND");
    let started_with = "https://cdn.test/mediatracks/abc?sig=FIRST";

    let id = crate::player::canonical_track_id(started_with);
    assert_eq!(
        refreshed_fetch_url(&id, Some(&retained)).as_deref(),
        Some("https://cdn.test/mediatracks/abc?sig=SECOND")
    );
}
