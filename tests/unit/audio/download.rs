//! Tests for `src/audio/download/mod.rs`, attached to it by `#[path]`.

use super::*;
use crate::audio::preload::tests::staged;

/// The downloader was written for the track being listened to and read that assumption
/// out of `CURRENT_TRACK`: every reconnect asked the global for a fresh credential and
/// stopped dead when the answer named someone else. Staging breaks that by definition
/// (`CURRENT_TRACK` names the OUTGOING track for a staged download's whole life); one
/// transient error retired the staged buffer for good instead of reconnecting it.
///
/// The traffic class carried the same assumption, with the opposite effect: staged bytes
/// charged to the playback queue skip the preload gate and its head allowance entirely,
/// and compete with the track being heard.
///
/// A preload arm that never looks at `CURRENT_TRACK` is also the premise that makes
/// `promote_crossfade` safe. It publishes the track BEFORE flipping the buffer's owner, and in
/// that window the buffer still reads `Preload` while its track is already current. Should this
/// arm ever start looking, the promotion order has to be revisited with it.
///
/// Neither assertion depends on what else holds `CURRENT_TRACK`: the playback arm answers
/// `None` whether the slot names another track or nothing at all, and the preload arm never
/// consults it. It still takes the slot fixture, because writing that global unguarded is
/// what raced a Windows test that DOES depend on it into failing.
#[test]
fn a_staged_download_keeps_its_own_url_while_another_track_plays() {
    let staged_url = "https://cdn.invalid/media/track-b.flac?token=STAGED";
    let staged_id = crate::player::canonical_track_id(staged_url);
    let _slot = crate::state::current_track_fixture::CurrentTrackSlot::holding(
        crate::state::RetainedTrack {
            track: staged("https://cdn.invalid/media/track-a.flac?token=PLAYING"),
            // Nothing here reads the generation: both arms answer on the canonical url alone.
            load_gen: 0,
        },
    );

    assert_eq!(
        DownloadOwner::Preload.fetch_url(&staged_id, staged_url),
        Some(staged_url.to_string()),
        "a staged download gave up reconnecting because the listener was on another track"
    );
    assert_eq!(
        DownloadOwner::Playback.fetch_url(&staged_id, staged_url),
        None,
        "a playback download must still stop once the track it fetches is not current"
    );
    assert!(
        matches!(
            DownloadOwner::Preload.traffic_class(),
            TrafficClass::Preload
        ),
        "staged bytes charged to the playback queue bypass the gate that paces them"
    );
}
