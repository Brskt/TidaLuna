//! Tests for `refreshed_fetch_url` in `src/player/mod.rs`, attached to it by `#[path]`.

use super::{refresh_retained_credential, refreshed_fetch_url};
use crate::state::TrackInfo;

fn track(url: &str) -> TrackInfo {
    TrackInfo {
        url: url.to_string(),
        key: String::new(),
        format: "flac".to_string(),
        product_id: None,
    }
}

/// The slot's shape, paired with the generation that published it. The credential tests below
/// never read that generation; the rule it obeys (a refresh is not a load, so it does not move)
/// has its own test at the end of this file.
fn retained_at(url: &str, load_gen: u32) -> Option<crate::state::RetainedTrack> {
    Some(crate::state::RetainedTrack {
        track: track(url),
        load_gen,
    })
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
    let mut retained = retained_at("https://cdn.test/mediatracks/abc?sig=OLD", 7);

    refresh_retained_credential(
        &mut retained,
        "https://cdn.test/mediatracks/abc",
        "https://cdn.test/mediatracks/abc?sig=NEW",
        "key",
        None,
    );

    let refreshed = retained.expect("still retained");
    assert_eq!(
        refreshed.track.url,
        "https://cdn.test/mediatracks/abc?sig=NEW"
    );
    assert_eq!(refreshed.track.key, "key");
}

/// The caller's guard reads the COMMITTED track, which the player thread sets, while this record is
/// written synchronously: a duplicate load can match the old committed track while a newer one is
/// already retained. Stamping the old credential on would strand the newer track's running download
/// on a canonical mismatch, and it dies silently.
#[test]
fn a_stale_load_cannot_overwrite_a_newer_tracks_credential() {
    let mut retained = retained_at("https://cdn.test/mediatracks/newer?sig=B", 7);

    refresh_retained_credential(
        &mut retained,
        "https://cdn.test/mediatracks/older",
        "https://cdn.test/mediatracks/older?sig=A",
        "key-a",
        None,
    );

    let untouched = retained.expect("still retained");
    assert_eq!(
        untouched.track.url, "https://cdn.test/mediatracks/newer?sig=B",
        "the newer track's credential must survive"
    );
    assert!(untouched.track.key.is_empty(), "and its key too");
}

/// The id refreshes with the credential, so a later replay of this source still knows its track.
#[test]
fn the_retained_id_is_refreshed_alongside_the_credential() {
    let mut retained = retained_at("https://cdn.test/mediatracks/abc?sig=OLD", 7);

    refresh_retained_credential(
        &mut retained,
        "https://cdn.test/mediatracks/abc",
        "https://cdn.test/mediatracks/abc?sig=NEW",
        "key",
        Some("120002099"),
    );

    assert_eq!(
        retained
            .expect("still retained")
            .track
            .product_id
            .as_deref(),
        Some("120002099")
    );
}

/// A load carrying no id must not blank the one already retained. A quality swap arrives without
/// one, and erasing it there cost the track its name for every replay that followed.
#[test]
fn a_load_without_an_id_leaves_the_retained_one_alone() {
    let mut retained = Some(crate::state::RetainedTrack {
        track: TrackInfo {
            product_id: Some("120002099".to_string()),
            ..track("https://cdn.test/mediatracks/abc?sig=OLD")
        },
        load_gen: 7,
    });

    refresh_retained_credential(
        &mut retained,
        "https://cdn.test/mediatracks/abc",
        "https://cdn.test/mediatracks/abc?sig=NEW",
        "key",
        None,
    );

    assert_eq!(
        retained
            .expect("still retained")
            .track
            .product_id
            .as_deref(),
        Some("120002099"),
        "an id-less load erased the identity a replay needs"
    );
}

/// A refresh re-signs a credential; it is not a new load, it mints no generation, and the
/// replay this source still authorises belongs to the generation that published it. Moving the
/// stamp here would hand a stale retained track the freshness of whatever load happened to be
/// current, which is exactly the pairing the slot exists to make impossible.
#[test]
fn a_credential_refresh_leaves_the_generation_where_it_was() {
    let mut retained = retained_at("https://cdn.test/mediatracks/abc?sig=OLD", 41);

    refresh_retained_credential(
        &mut retained,
        "https://cdn.test/mediatracks/abc",
        "https://cdn.test/mediatracks/abc?sig=NEW",
        "key",
        Some("120002099"),
    );

    let refreshed = retained.expect("still retained");
    assert_eq!(
        refreshed.track.url, "https://cdn.test/mediatracks/abc?sig=NEW",
        "the refresh must still take the new credential"
    );
    assert_eq!(
        refreshed.load_gen, 41,
        "a credential refresh moved the generation, so a stale source can now pass a replay guard"
    );
}

/// Equality answers "is this the same source", which is the question a preload hit asks. The
/// preload delegate is handed no id, so the preloaded copy carries none while the load that comes
/// to claim it carries the real one. Comparing ids here would make every gapless hit miss.
#[test]
fn two_records_of_one_source_match_whether_or_not_they_name_the_track() {
    let preloaded = track("https://cdn.test/mediatracks/abc?sig=A");
    let claimed = TrackInfo {
        product_id: Some("120002099".to_string()),
        ..track("https://cdn.test/mediatracks/abc?sig=A")
    };

    assert_eq!(
        preloaded, claimed,
        "a preload hit stopped matching once the load carried an id"
    );
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
