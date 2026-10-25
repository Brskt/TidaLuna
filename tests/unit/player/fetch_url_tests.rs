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

/// The slot's shape, paired with the generation that published it.
fn retained_at(url: &str, load_gen: u32) -> Option<crate::state::RetainedTrack> {
    Some(crate::state::RetainedTrack {
        track: track(url),
        load_gen,
    })
}

/// The signed query is refreshed under a running download and the reconnect must use the new one:
/// keeping the captured copy 403'd a track dead after its signature lapsed. A task whose track is
/// no longer the retained one, or none retained, gets nothing: it would download ANOTHER track's
/// audio into this track's buffer.
#[test]
fn a_running_download_gets_the_refreshed_signature_only_for_its_own_retained_track() {
    for (retained, expected) in [
        (
            Some(track("https://cdn.test/mediatracks/abc?sig=FRESH")),
            Some("https://cdn.test/mediatracks/abc?sig=FRESH"),
        ),
        (
            Some(track("https://cdn.test/mediatracks/other?sig=FRESH")),
            None,
        ),
        (None, None),
    ] {
        assert_eq!(
            refreshed_fetch_url("https://cdn.test/mediatracks/abc", retained.as_ref()).as_deref(),
            expected,
            "{retained:?}"
        );
    }
}

/// The refresh takes the new signature for the track it was asked about, and the id with it, so that a
/// later replay of this source still knows its track. It is not a new load: it mints no generation,
/// and moving the stamp would hand a stale retained track the freshness of whatever load is current.
#[test]
fn a_credential_refresh_takes_the_new_signature_and_id_and_leaves_the_generation() {
    for id in [None, Some("120002099")] {
        let mut retained = retained_at("https://cdn.test/mediatracks/abc?sig=OLD", 41);

        refresh_retained_credential(
            retained.as_mut(),
            "https://cdn.test/mediatracks/abc",
            "https://cdn.test/mediatracks/abc?sig=NEW",
            "key",
            id,
        );

        let refreshed = retained.expect("still retained");
        assert_eq!(
            refreshed.track.url, "https://cdn.test/mediatracks/abc?sig=NEW",
            "id {id:?}"
        );
        assert_eq!(refreshed.track.key, "key", "id {id:?}");
        assert_eq!(refreshed.track.product_id.as_deref(), id, "id {id:?}");
        assert_eq!(
            refreshed.load_gen, 41,
            "a credential refresh moved the generation; a stale source can now pass a replay guard"
        );
    }
}

/// The caller's guard reads the COMMITTED track, which the player thread sets, while this record is
/// written synchronously: a duplicate load can match the old committed track while a newer one is
/// already retained. Stamping the old credential on would strand the newer track's running download
/// on a canonical mismatch, and it dies silently.
#[test]
fn a_stale_load_cannot_overwrite_a_newer_tracks_credential() {
    let mut retained = retained_at("https://cdn.test/mediatracks/newer?sig=B", 7);

    refresh_retained_credential(
        retained.as_mut(),
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
        retained.as_mut(),
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
