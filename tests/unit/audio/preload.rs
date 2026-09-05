//! Tests for `src/audio/preload.rs`, attached to it by `#[path]`.

use super::*;
use crate::state::{PreloadedTrack, TrackInfo};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
/// libtest runs the crate in one process, several threads wide, and `PRELOAD_STATE`
/// is process-wide: a second test touching it would race this one in silence. Take
/// this first, the way `tests/unit/logging.rs` does for its own globals, as a tokio
/// mutex, because unlike that one this guard is held across an `.await`.
pub(crate) static PRELOAD_TESTS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn staged(url: &str) -> TrackInfo {
    TrackInfo {
        url: url.to_string(),
        key: "key".to_string(),
        format: "flac".to_string(),
        product_id: Some("product".to_string()),
    }
}

/// Both halves live in one test on purpose: `PRELOAD_STATE` is process-wide, and two
/// tests writing it would interleave under the parallel harness.
///
/// The first half is the defect this exists for. The staged record and the load that
/// supersedes it name the same track through DIFFERENT signed urls, because the url
/// is a per-request credential: an equality over `TrackInfo` (url, key, format)
/// would not match, leave the record standing, and hand the crossfade the track that
/// is already playing. The second half is the other half of the contract: a
/// genuinely different next track must survive its predecessor's load, or every
/// transition loses its preload.
///
/// The `begin_load` between staging and clearing is what makes the record STALE, and it
/// is load-bearing: without it the record carries the very generation the clear runs
/// under, which is the deliberate case its sibling below pins.
#[tokio::test]
async fn a_load_clears_the_staged_record_that_names_it_and_spares_the_others() {
    let _serialised = PRELOAD_TESTS.lock().await;
    preload_state().stage_for_test(staged(
        "https://cdn.invalid/media/track-a.flac?token=STAGED&exp=1",
    ));
    crate::player::begin_load(crate::player::LoadOrigin::Local);

    clear_next_track_if_match("https://cdn.invalid/media/track-a.flac").await;

    assert!(
        preload_state().staged_next().is_none(),
        "a record staged before this load began is stale, however its url was signed"
    );

    let other = staged("https://cdn.invalid/media/track-b.flac?token=STAGED");
    preload_state().stage_for_test(other.clone());

    clear_next_track_if_match("https://cdn.invalid/media/track-a.flac").await;

    assert_eq!(
        preload_state().staged_next(),
        Some(&other),
        "clearing on a different track's load would destroy every genuine preload"
    );

    preload_state().reset();
}

/// The half its sibling above cannot pin: a record staged AFTER this load began belongs to
/// it, and the clear that load performs must leave it alone.
///
/// Repeat-one is the only thing that produces this state (the renderer restages the track
/// now playing, under the load now current), and the clear used to key on the canonical id
/// alone: it destroyed that record as though it predated the load it was staged for. The
/// completion path then found nothing to advance to, and the SDK, already told it had a
/// preloaded item, transitioned into silence rather than replaying.
#[tokio::test]
async fn a_load_spares_the_staged_record_that_belongs_to_it() {
    let _serialised = PRELOAD_TESTS.lock().await;
    crate::player::begin_load(crate::player::LoadOrigin::Local);
    let repeat = staged("https://cdn.invalid/media/track-a.flac?token=RESTAGED");
    preload_state().stage_for_test(repeat.clone());

    clear_next_track_if_match("https://cdn.invalid/media/track-a.flac").await;

    assert_eq!(
        preload_state().staged_next(),
        Some(&repeat),
        "repeat-one restaged this track under the load now clearing, and lost it to an id match"
    );

    preload_state().reset();
}

/// A claim must weigh whether the attempt it would defer to is ALIVE, not merely present.
///
/// `stage_streaming` returns the moment the head lands. The tracked task finishes long
/// before the transfer does; the transfer itself runs in a task this state never named. When it
/// dies, `data` keeps its corpse and nothing tells this module. Reading occupancy as liveness
/// therefore refused every later claim for that track and restamped the corpse as fresh,
/// where a reset would have retired it and staged the track again from scratch.
#[tokio::test]
async fn a_claim_over_a_dead_download_restages_instead_of_restamping() {
    let _serialised = PRELOAD_TESTS.lock().await;
    let track = staged("https://cdn.invalid/media/track-a.flac?token=ONE");
    let first = preload_state()
        .claim(&track, crate::player::current_gen())
        .expect("nothing is staged, so the claim is granted");

    let (buffer, writer) = crate::player::buffer::RamBuffer::new(
        1_000_000,
        DownloadOwner::Preload,
        CancellationToken::new(),
    );
    assert!(
        preload_state().publish(first, &track, buffer),
        "the attempt still owns the slot, so its own publish must land"
    );
    // Past the head, hence past the one exit `stage_streaming` has for a download that ends
    // early: that task has already returned and nobody is left watching this transfer.
    writer.finish_with_error(DownloadFailure::Network, "connection gone".to_string());

    let second = preload_state()
        .claim(&track, crate::player::current_gen())
        .expect("the staged download is dead, so the claim must re-stage rather than defer");

    assert_ne!(
        second, first,
        "a dead attempt was restamped as live, and the track was never fetched again"
    );

    preload_state().reset();
}

/// The capped mode is no longer what the preload normally uses: it stages a stream and
/// has no ceiling. This covers the one path that still reaches it: a server that
/// announces no length at all, where a whole-copy fetch is the only option and nothing
/// else bounds the body.
///
/// Within that mode the refusal still has to come from the announced length rather than
/// from what has arrived. This server promises a body far past the cap and then sends
/// none of it. A fetch that reached for the bytes before deciding would spend its read
/// timeout and come back an error, which is what the assertion below distinguishes.
#[tokio::test]
async fn an_over_size_body_is_refused_from_its_announced_length() {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut request = [0u8; 1024];
        let _ = sock.read(&mut request).await;
        let announced = PRELOAD_MAX_BYTES + 1;
        sock.write_all(
            format!("HTTP/1.1 200 OK\r\nContent-Length: {announced}\r\n\r\n").as_bytes(),
        )
        .await
        .unwrap();
        sock.flush().await.unwrap();
        // Never send the body: reaching for it is the failure this test names.
        std::future::pending::<()>().await;
    });

    let fetched = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        fetch_and_decrypt_inner(
            &format!("http://127.0.0.1:{port}/track.flac"),
            "",
            Some(PRELOAD_MAX_BYTES),
            DownloadOwner::Preload,
            // The refusal must decide on the header alone. The fetch opens its own
            // response rather than being handed one.
            None,
        ),
    )
    .await
    .expect("deciding on the header alone needs no body, so this cannot wait on one")
    .expect("an over-size track is a refusal to stage, not a failed fetch");

    assert!(
        fetched.is_none(),
        "a track whose announced size passes the cap must not be staged"
    );
}

/// `data` carries the bytes: committing one track must not spend the record another
/// transition owns: a staged copy reaches the disk cache only after its own track has
/// been current, and dropping it here would take the ciphertext with it.
///
/// The first half is the sparing. The second is the other half of the contract, and it
/// is why the guard reads a canonical id rather than the source: the same track staged
/// again carries a freshly signed url, and it still has to clear.
#[tokio::test]
async fn committing_spares_a_staged_record_that_names_another_track() {
    let _serialised = PRELOAD_TESTS.lock().await;
    let elsewhere = staged("https://cdn.invalid/media/track-b.flac?token=STAGED");
    PRELOAD_STATE.lock().unwrap_or_else(|e| e.into_inner()).data = Some(PreloadedTrack {
        track: elsewhere.clone(),
        buffer: crate::player::buffer::RamBuffer::from_complete(Vec::new()),
    });

    commit_peeked(&staged(
        "https://cdn.invalid/media/track-a.flac?token=COMMITTED",
    ));

    assert_eq!(
        PRELOAD_STATE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .data
            .as_ref()
            .map(|d| &d.track),
        Some(&elsewhere),
        "committing one track spent the bytes staged for a different one"
    );

    PRELOAD_STATE.lock().unwrap_or_else(|e| e.into_inner()).data = Some(PreloadedTrack {
        track: staged("https://cdn.invalid/media/track-b.flac?token=STAGED&exp=1"),
        buffer: crate::player::buffer::RamBuffer::from_complete(Vec::new()),
    });

    commit_peeked(&staged(
        "https://cdn.invalid/media/track-b.flac?token=FRESH&exp=2",
    ));

    assert!(
        PRELOAD_STATE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .data
            .is_none(),
        "the committed track's own bytes are spent, however its url was signed"
    );

    PRELOAD_STATE.lock().unwrap_or_else(|e| e.into_inner()).data = None;
}

/// Both halves of the failure contract, in one test because `PRELOAD_STATE` is
/// process-wide.
///
/// The first is the defect: a staged copy its decoder gave up on used to stay in `data`;
/// the fade re-armed on the same bytes every poll, and the hard cut meant to rescue the
/// transition took them as a preload hit. The next track then failed twice instead of
/// playing.
///
/// The second is the contract that must survive the fix. `next_track` names what the cut
/// advances to; clearing it here would leave the completion path with nothing, which is
/// the failure the sibling test in `player/thread/commands.rs` pins from the other side.
#[tokio::test]
async fn a_staged_copy_its_decoder_refused_is_dropped_and_the_next_track_stands() {
    let _serialised = PRELOAD_TESTS.lock().await;
    let failed = staged("https://cdn.invalid/media/track-a.flac?token=STAGED");
    {
        let mut lock = preload_state();
        lock.data = Some(PreloadedTrack {
            track: failed.clone(),
            buffer: crate::player::buffer::RamBuffer::from_complete(Vec::new()),
        });
        lock.stage_for_test(failed.clone());
    }

    discard_staged_if_match(&failed);

    {
        let lock = PRELOAD_STATE.lock().unwrap_or_else(|e| e.into_inner());
        assert!(
            lock.data.is_none(),
            "the copy that already failed to decode was left for the cut to fail on again"
        );
        assert_eq!(
            lock.staged_next(),
            Some(&failed),
            "the cut has nothing to advance to once its next track is gone"
        );
    }

    // A record staged for another track is an untried copy: the failure says nothing
    // about it.
    let elsewhere = staged("https://cdn.invalid/media/track-b.flac?token=STAGED");
    PRELOAD_STATE.lock().unwrap_or_else(|e| e.into_inner()).data = Some(PreloadedTrack {
        track: elsewhere.clone(),
        buffer: crate::player::buffer::RamBuffer::from_complete(Vec::new()),
    });

    discard_staged_if_match(&failed);

    assert_eq!(
        PRELOAD_STATE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .data
            .as_ref()
            .map(|d| &d.track),
        Some(&elsewhere),
        "one track's decode failure spent the bytes staged for a different one"
    );

    preload_state().reset();
}

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

/// The head-wait exit is the one return that never publishes the slot. Nothing can
/// ever read what the download keeps writing. Left running it accumulates the whole
/// track (this path lost its size ceiling when it became a streaming one), and none of
/// it is recoverable: the parked ciphertext reaches the disk cache only through the
/// buffer that becomes `current_buffer`, and this one never does. The bandwidth is
/// spent twice over, against the track being listened to, for bytes that are then
/// dropped.
///
/// Costs the full head timeout in wall clock: the deadline is a `std::time::Instant`,
/// which tokio's test clock cannot advance.
#[tokio::test]
async fn a_head_that_never_arrives_stops_its_download() {
    let _serialised = PRELOAD_TESTS.lock().await;
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut request = [0u8; 1024];
        let _ = sock.read(&mut request).await;
        // A length far past the head target, then a body that stops short of it.
        sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1048576\r\n\r\n")
            .await
            .unwrap();
        sock.write_all(&[0u8; 1024]).await.unwrap();
        sock.flush().await.unwrap();
        std::future::pending::<()>().await;
    });

    let track = TrackInfo {
        url: format!("http://127.0.0.1:{port}/track.flac"),
        key: String::new(),
        format: "flac".to_string(),
        product_id: Some("product".to_string()),
    };
    // Staging answers to an attempt now. The test claims one the way `start_preload` does:
    // without it, nothing would own the slot and the token would never be recorded at all.
    let attempt = preload_state()
        .claim(&track, crate::player::current_gen())
        .expect("nothing is staged, so the claim is granted");
    stage_streaming(&track, attempt)
        .await
        .expect("a head that does not arrive is a refusal to stage, not a failed fetch");

    let lock = PRELOAD_STATE.lock().unwrap_or_else(|e| e.into_inner());
    assert!(
        lock.data.is_none(),
        "nothing may be staged when the head never arrived"
    );
    assert!(
        lock.download_cancel
            .as_ref()
            .expect("the streaming branch records its token before downloading")
            .is_cancelled(),
        "the download outlived the only scope that could have read it"
    );
}

/// A staging download that ENDS before its head has to give the slot back at once.
///
/// This wait asks the token, and the token answers "someone asked me to stop", which a
/// download that ended on its own never says. It used to spend the whole deadline, and
/// the record published before it stayed adoptable for all eight seconds: a load taking it
/// gets a buffer that can never become playable, and pays its own wait to find out.
///
/// Driven through the 416 route because that is the one ending CLEANLY under the announced
/// length: a body cut short, then a reconnect the server answers "nothing left". No error
/// is ever set: `finished` alone says it is over, the case a token cannot see.
#[tokio::test]
async fn a_download_that_ends_before_its_head_gives_the_slot_back_at_once() {
    let _serialised = PRELOAD_TESTS.lock().await;
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let mut request = [0u8; 1024];
        // A length far past the head target, a body that stops short of it, then a close.
        // Against a declared length that is a framing error. The download reconnects.
        let (mut sock, _) = listener.accept().await.unwrap();
        let _ = sock.read(&mut request).await;
        sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1048576\r\n\r\n")
            .await
            .unwrap();
        sock.write_all(&[0u8; 1024]).await.unwrap();
        sock.flush().await.unwrap();
        drop(sock);
        // The reconnect is told the resource ends where the body already stopped, which the
        // download reads as a complete transfer: park what it has and finish.
        let (mut sock, _) = listener.accept().await.unwrap();
        let _ = sock.read(&mut request).await;
        sock.write_all(b"HTTP/1.1 416 Range Not Satisfiable\r\nContent-Length: 0\r\n\r\n")
            .await
            .unwrap();
        sock.flush().await.unwrap();
    });

    let track = TrackInfo {
        url: format!("http://127.0.0.1:{port}/track.flac"),
        key: String::new(),
        format: "flac".to_string(),
        product_id: Some("product".to_string()),
    };
    let attempt = preload_state()
        .claim(&track, crate::player::current_gen())
        .expect("nothing is staged, so the claim is granted");

    let staged_at = std::time::Instant::now();
    stage_streaming(&track, attempt)
        .await
        .expect("a download that ended is a refusal to stage, not a failed fetch");

    // Well under `HEAD_WAIT_TIMEOUT_MS`, and clear of the reconnect backoff below it.
    assert!(
        staged_at.elapsed() < std::time::Duration::from_secs(3),
        "staging waited out its head deadline on a download that had already ended"
    );

    let lock = PRELOAD_STATE.lock().unwrap_or_else(|e| e.into_inner());
    assert!(
        lock.data.is_none(),
        "a published record whose download is over must not stay adoptable"
    );
    assert!(
        lock.download_cancel
            .as_ref()
            .expect("the streaming branch records its token before downloading")
            .is_cancelled(),
        "the slot went back while its token still claimed a download to stop"
    );
}

/// A load for the track being staged adopts the download already in flight.
///
/// This is the defect the change exists for. The slot used to be published only once the head
/// had landed. A load arriving inside that window (up to eight seconds) found nothing,
/// opened its own fetch of the same file, and left the staged one running to the end. One
/// track, two complete downloads, seen in a real session.
#[tokio::test]
async fn a_load_adopts_a_staging_download_before_its_head_lands() {
    let _serialised = PRELOAD_TESTS.lock().await;
    // Announces a large body, then sends far less than a head and stops: it holds the window
    // the defect lived in open for the length of the test.
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut request = [0u8; 1024];
        let _ = sock.read(&mut request).await;
        sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1048576\r\n\r\n")
            .await
            .unwrap();
        sock.write_all(&[0u8; 1024]).await.unwrap();
        sock.flush().await.unwrap();
        std::future::pending::<()>().await;
    });

    let track = TrackInfo {
        url: format!("http://127.0.0.1:{port}/track.flac"),
        key: String::new(),
        format: "flac".to_string(),
        product_id: Some("product".to_string()),
    };
    let attempt = preload_state()
        .claim(&track, crate::player::current_gen())
        .expect("nothing is staged, so the claim is granted");

    let staging = tokio::spawn({
        let track = track.clone();
        async move {
            let _ = stage_streaming(&track, attempt).await;
        }
    });

    // The load arrives while the head is still missing. Polled rather than awaited because the
    // claim under test is about WHEN the slot appears: as soon as the download starts, not
    // after the head wait it used to sit behind.
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(2000);
    let mut adopted = None;
    while std::time::Instant::now() < deadline {
        adopted = take_preloaded_if_match(&track).await;
        if adopted.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    staging.abort();

    let adopted =
        adopted.expect("the load found nothing and had to open a second download of the same file");
    assert!(
        adopted.buffer.written() < HEAD_TARGET_BYTES,
        "the head landed after all, so this run did not exercise the window it was written for"
    );

    preload_state().reset();
}

/// A staged "next" belongs to the load that asked for it and to no other.
///
/// Nothing used to say so: the completion path took whatever was in the slot. A record left by
/// a local queue could therefore be picked up and played in the middle of a TIDAL Connect
/// session, and every event of that track was then relayed to a controller that had never
/// asked for it, stamped with the engine generation it no longer belonged to.
#[tokio::test]
async fn a_next_track_from_a_superseded_load_is_not_promoted() {
    let _serialised = PRELOAD_TESTS.lock().await;
    let orphan = staged("https://cdn.invalid/media/track-a.flac?token=STAGED");
    {
        preload_state().stage_for_test(orphan.clone());
    }

    // Another load begins (a skip or a stop), minted under the same origin: this isolates
    // the generation dimension. The origin dimension has its own test below.
    crate::player::begin_load(crate::player::LoadOrigin::Local);

    assert!(
        take_next_track().await.is_none(),
        "the record named a queue nobody is playing any more"
    );
    assert!(
        peek_next_track().is_none(),
        "and no fade may arm into it either: same defect, other reader"
    );
}

/// The stamp says WHEN a record was staged and never WHO the load it answers to belongs to.
///
/// A preload the renderer issues AFTER a Connect load has taken over stamps itself with that
/// load's own generation. The staleness guard above has nothing to catch: it reads as
/// perfectly fresh. Promoted, the speaker fades (or cuts) into a track chosen by a queue
/// that is not driving, and the controller is told it picked something it never did.
#[tokio::test]
async fn a_record_staged_under_another_origins_load_is_not_promoted() {
    let _serialised = PRELOAD_TESTS.lock().await;

    // Connect takes over. Its load owns the generation the renderer stamps itself with.
    crate::player::begin_load(crate::player::LoadOrigin::Connect);
    preload_state().stage_for_test(staged(
        "https://cdn.invalid/media/track-a.flac?token=STAGED",
    ));

    let refused = take_next_track().await.is_none() && peek_next_track().is_none();

    // Process-wide, like the generation it now shares a word with: hand the local origin back
    // before asserting; a failure here must not leave every later test refusing its own preload.
    crate::player::begin_load(crate::player::LoadOrigin::Local);

    assert!(
        refused,
        "the renderer's own queue promoted a track into a session it does not drive"
    );
}

/// The guard must not break repeat-one, where `next_track` names the track that is PLAYING on
/// purpose: the replay is the whole mechanism. It re-stages under the same load with no load
/// in between; the stamp still matches and the promotion goes ahead.
#[tokio::test]
async fn repeat_one_still_promotes_the_track_it_restaged() {
    let _serialised = PRELOAD_TESTS.lock().await;
    let again = staged("https://cdn.invalid/media/track-a.flac?token=STAGED");
    {
        preload_state().stage_for_test(again.clone());
    }

    assert_eq!(
        take_next_track().await.as_ref(),
        Some(&again),
        "nothing superseded it, so it still answers the load that is current"
    );
}

/// A complete staged buffer carrying a ciphertext, the shape a finished preload leaves behind.
fn staged_with_ciphertext() -> crate::player::buffer::RamBuffer {
    let file = tempfile::NamedTempFile::new().expect("a temp file for the staged ciphertext");
    crate::player::buffer::RamBuffer::from_complete_with_ciphertext(
        b"audio".to_vec(),
        Some((file, 5)),
    )
}

/// The two removals of `data` have opposite reasons, and the cache must not be handed the
/// wrong one.
///
/// `cancel_preload` drops a staged track the listener never reached. Its download and its
/// decrypt are already paid for, and the bytes were lost for one reason only: nothing but the
/// buffer that becomes `current_buffer` was ever read for a ciphertext. Those belong in the
/// cache, or the next visit to that track refetches all of it.
///
/// `discard_staged_if_match` drops a copy a decoder has just REJECTED. Caching those would
/// hand the same failure back from disk on every later attempt. They have to die with the
/// record, and a fix that treated both removals alike would do exactly that.
#[tokio::test]
async fn a_preload_never_reached_is_cached_where_a_refused_one_is_not() {
    let _serialised = PRELOAD_TESTS.lock().await;

    let unplayed = staged("https://cdn.invalid/media/track-a.flac?token=STAGED");
    let unplayed_buffer = staged_with_ciphertext();
    {
        let mut lock = PRELOAD_STATE.lock().unwrap_or_else(|e| e.into_inner());
        lock.data = Some(PreloadedTrack {
            track: unplayed.clone(),
            buffer: unplayed_buffer.clone(),
        });
    }

    cancel_preload().await;

    assert!(
        unplayed_buffer.take_ciphertext().is_none(),
        "the bytes were handed to the cache rather than dying with the staged record"
    );

    let refused = staged("https://cdn.invalid/media/track-b.flac?token=STAGED");
    let refused_buffer = staged_with_ciphertext();
    {
        let mut lock = PRELOAD_STATE.lock().unwrap_or_else(|e| e.into_inner());
        lock.data = Some(PreloadedTrack {
            track: refused.clone(),
            buffer: refused_buffer.clone(),
        });
    }

    discard_staged_if_match(&refused);

    assert!(
        refused_buffer.take_ciphertext().is_some(),
        "a copy the decoder refused must not reach the cache: the cut would fail on it again"
    );

    PRELOAD_STATE.lock().unwrap_or_else(|e| e.into_inner()).data = None;
}

/// Published and usable are two different things, and the two readers need different ones.
///
/// The bytes are published the moment they start arriving, for a load to adopt the download
/// already running. A fade cannot take that same buffer: `arm_crossfade` probes it on the
/// PLAYER thread with a blocking read, and one whose head has not landed parks there for up to
/// thirty seconds of frozen transport. One slot, two answers.
#[tokio::test]
async fn an_unfilled_buffer_serves_a_load_but_never_a_fade() {
    let _serialised = PRELOAD_TESTS.lock().await;
    let track = staged("https://cdn.invalid/media/track-a.flac?token=STAGED");
    let attempt = preload_state()
        .claim(&track, crate::player::current_gen())
        .expect("nothing is staged, so the claim is granted");

    // What `stage_streaming` publishes the instant its download starts: a real buffer with no
    // bytes in it yet. The writer stays bound: dropping it would finish the buffer.
    let (buffer, _writer) = crate::player::buffer::RamBuffer::new(
        1_000_000,
        DownloadOwner::Preload,
        CancellationToken::new(),
    );
    assert!(
        preload_state().publish(attempt, &track, buffer),
        "the attempt owns the slot, so its own publish must land"
    );

    assert!(
        peek_preloaded().is_none(),
        "a fade armed on a head that has not landed freezes the player thread for thirty seconds"
    );
    assert!(
        take_preloaded_if_match(&track).await.is_some(),
        "the load that could have reused this download was sent to open a second one"
    );

    preload_state().reset();
}

/// The load path's own head guard, and why it waits instead of refusing.
///
/// `peek_data` holds a fade back from a buffer whose head has not landed. The load path is the
/// other consumer of the same slot and had no guard at all: it handed the buffer straight to
/// the player thread, which probes it with a BLOCKING read and parks there for up to thirty
/// seconds (no time updates, no seek, no pause) for a load arriving in the window between a
/// download being published and its first bytes arriving.
///
/// Refusing would have been the wrong shape: it sends the load to open a SECOND download of
/// bytes already on their way, which is what publishing early exists to prevent. The record is
/// still taken the instant it matches; only the handover to the player thread waits.
#[tokio::test]
async fn a_load_waits_for_the_head_before_the_player_thread_sees_the_buffer() {
    let _serialised = PRELOAD_TESTS.lock().await;
    let (buffer, writer) = crate::player::buffer::RamBuffer::new(
        1_000_000,
        DownloadOwner::Preload,
        CancellationToken::new(),
    );

    // A load nobody is waiting for any more stops at once rather than spending the deadline.
    let gave_up_at = std::time::Instant::now();
    assert!(
        !crate::audio::preload::head_has_landed(&buffer, || false).await,
        "a superseded load must not wait for a head it will never use"
    );
    assert!(
        gave_up_at.elapsed() < std::time::Duration::from_millis(500),
        "the wait ran on after its caller had gone"
    );

    // The head lands while the load is waiting: the wait is what makes the buffer probeable.
    let feeder = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        writer.write_counted(&vec![0u8; HEAD_TARGET_BYTES as usize]);
    });
    assert!(
        crate::audio::preload::head_has_landed(&buffer, || true).await,
        "the head arrived, so the load must proceed with the copy it adopted"
    );
    assert!(
        buffer.written() >= HEAD_TARGET_BYTES,
        "the load was let through on a buffer the player thread would have blocked on"
    );
    feeder.await.unwrap();
}

/// A wait for the head ends when no further byte can arrive, whatever ended the download.
///
/// All three causes below leave `written` short of the target for good, and the wait used to
/// consult neither of the flags that say so: it spent the whole `HEAD_WAIT_TIMEOUT_MS` on
/// each of them before falling back to the ordinary load. That is eight seconds of silence
/// between two tracks, with the outgoing one already stopped.
///
/// One test for the three because they are one behaviour asked of three states: a failure
/// and a clean end both latch `finished`, while a writer dropped mid-flight latches
/// `cancelled` instead, and a wait that reads only one of the two answers half the class.
#[tokio::test]
async fn a_terminal_download_stops_the_head_wait_however_it_ended() {
    let short = (HEAD_TARGET_BYTES / 4) as usize;
    let new_buffer = || {
        crate::player::buffer::RamBuffer::new(
            1_000_000,
            DownloadOwner::Preload,
            CancellationToken::new(),
        )
    };

    let (failed, writer) = new_buffer();
    writer.write_counted(&vec![0u8; short]);
    writer.finish_with_error(DownloadFailure::Network, "connection gone".to_string());
    let waited = std::time::Instant::now();
    assert!(
        !crate::audio::preload::head_has_landed(&failed, || true).await,
        "a download that died cannot deliver the head it is being waited on for"
    );
    assert!(
        waited.elapsed() < std::time::Duration::from_millis(500),
        "the wait sat out its deadline on a buffer whose failure was already known"
    );

    // A body that ends cleanly under its announced length announces nothing at all: no error
    // is set, and `written` simply stops. Only `finished` tells the waiter it is over.
    let (short_body, writer) = new_buffer();
    writer.write_counted(&vec![0u8; short]);
    writer.finish();
    let waited = std::time::Instant::now();
    assert!(
        !crate::audio::preload::head_has_landed(&short_body, || true).await,
        "a body that ended short of the head target will not grow past it"
    );
    assert!(
        waited.elapsed() < std::time::Duration::from_millis(500),
        "a clean end left the wait believing bytes were still coming"
    );

    let (abandoned, writer) = new_buffer();
    writer.write_counted(&vec![0u8; short]);
    drop(writer);
    let waited = std::time::Instant::now();
    assert!(
        !crate::audio::preload::head_has_landed(&abandoned, || true).await,
        "the writer is gone, so the head can no longer arrive"
    );
    assert!(
        waited.elapsed() < std::time::Duration::from_millis(500),
        "a dropped writer left the wait parked on a producer that no longer exists"
    );
}

/// The head landing and the download ending are not exclusive, and the order the two are
/// read in decides whether a good buffer is refused.
///
/// A writer that lands its last chunk and then ends leaves both true at once. Bytes have to
/// win: the caller asked whether the head is probeable, and it is. This pins the answer
/// against a wait that would ask "is it over?" first and refuse a head already in hand.
#[tokio::test]
async fn a_head_that_lands_as_its_download_ends_is_still_a_hit() {
    let (buffer, writer) = crate::player::buffer::RamBuffer::new(
        HEAD_TARGET_BYTES,
        DownloadOwner::Preload,
        CancellationToken::new(),
    );
    writer.write_counted(&vec![0u8; HEAD_TARGET_BYTES as usize]);
    writer.finish();

    assert!(
        crate::audio::preload::head_has_landed(&buffer, || true).await,
        "the head is there to be probed, so the end of the download is not a refusal"
    );
}

/// A refusal has to re-stamp the record it refuses, or one skip disables the fade for good.
///
/// The guard exists to stop a second producer naming the same next track from restarting a
/// download the first one has already begun. It used to return without touching the stamp:
/// once an unrelated load advanced the counter the record read as superseded, and could never
/// heal, because the same guard refused every later call that would have re-stamped it.
#[tokio::test]
async fn a_refused_claim_still_restamps_the_record_it_refuses() {
    let _serialised = PRELOAD_TESTS.lock().await;
    let track = staged("https://cdn.invalid/media/track-a.flac?token=ONE");
    let first = preload_state()
        .claim(&track, crate::player::current_gen())
        .expect("nothing is staged, so the claim is granted");
    // What the staging task would have published. It is also what keeps the guard shut.
    preload_state().publish(
        first,
        &track,
        crate::player::buffer::RamBuffer::from_complete(Vec::new()),
    );

    // An unrelated load begins: a Connect handover, a skip, a stop.
    crate::player::begin_load(crate::player::LoadOrigin::Local);

    // The other producer names the same next track again, under the load that is now current.
    assert!(
        preload_state()
            .claim(&track, crate::player::current_gen())
            .is_none(),
        "a live attempt already covers this track, so the second call must not restart it"
    );

    assert!(
        peek_next_track().is_some(),
        "the refusal left the stamp of a load that is gone, so every reader now rejects a staged copy that is still good"
    );
    preload_state().reset();
}

/// Dropping a superseded record has to take the download feeding it, not just its name.
///
/// The name alone left an uncapped download running for a track nobody would play, competing
/// for bandwidth with the one being listened to and holding its buffer until some unrelated
/// later preload happened to sweep it.
#[tokio::test]
async fn a_superseded_record_takes_its_download_with_it() {
    let _serialised = PRELOAD_TESTS.lock().await;
    let orphan = staged("https://cdn.invalid/media/track-a.flac?token=STAGED");
    let cancel = CancellationToken::new();
    {
        let mut lock = preload_state();
        lock.stage_for_test(orphan.clone());
        lock.data = Some(PreloadedTrack {
            track: orphan.clone(),
            buffer: crate::player::buffer::RamBuffer::from_complete(Vec::new()),
        });
        lock.download_cancel = Some(cancel.clone());
    }

    crate::player::begin_load(crate::player::LoadOrigin::Local);

    assert!(
        take_next_track().await.is_none(),
        "the record named a queue nobody is playing any more"
    );
    assert!(
        cancel.is_cancelled(),
        "its download ran on, uncapped, against the track actually playing"
    );
    assert!(
        preload_state().data.is_none(),
        "and its buffer stayed in memory waiting for an unrelated preload to sweep it"
    );
    preload_state().reset();
}

/// The name is spent on the identity of the committed track, not on how its url was signed.
///
/// A full-equality compare reads as stricter and is simply wrong here: the signed url is a
/// credential; a copy re-staged under a fresh one left the name standing while its bytes
/// were spent. Both readers then acted on a record naming the track that is already playing:
/// the fade arms into itself, the completion path reloads instead of advancing.
#[tokio::test]
async fn committing_spends_the_name_however_the_url_was_signed() {
    let _serialised = PRELOAD_TESTS.lock().await;
    preload_state().stage_for_test(staged(
        "https://cdn.invalid/media/track-a.flac?token=STAGED&exp=1",
    ));

    commit_peeked(&staged(
        "https://cdn.invalid/media/track-a.flac?token=FRESH&exp=2",
    ));

    assert!(
        preload_state().staged_next().is_none(),
        "the name outlived the track it names, and the readers act on it"
    );
    preload_state().reset();
}

/// A load of the very track being staged must not cost that staging its right to publish.
///
/// The publish gate used to read the staged NAME, which the ordinary load path clears on its
/// way past for exactly this track. The bytes already arriving were then thrown away and the
/// load refetched the whole track in parallel with the download it had just orphaned.
#[tokio::test]
async fn a_load_of_the_track_being_staged_does_not_cost_it_its_bytes() {
    let _serialised = PRELOAD_TESTS.lock().await;
    let track = staged("https://cdn.invalid/media/track-a.flac?token=STAGED");
    let attempt = preload_state()
        .claim(&track, crate::player::current_gen())
        .expect("nothing is staged, so the claim is granted");

    clear_next_track_if_match("https://cdn.invalid/media/track-a.flac").await;

    assert!(
        preload_state().publish(
            attempt,
            &track,
            crate::player::buffer::RamBuffer::from_complete(Vec::new())
        ),
        "the staging task lost its slot to the load that was about to ask for its bytes"
    );
    preload_state().reset();
}

/// Only the attempt that currently owns the slot may write into it.
///
/// `start_preload` cannot hold the lock across its awaits. The handles it produces arrive
/// after its decision. Named by their attempt, a superseded one's handles are refused; unnamed,
/// they overwrote whatever the winner had installed, leaving the surviving token and task
/// naming a different track than the record did, which is how cancel-on-skip stops reaching
/// the download it means to stop.
#[tokio::test]
async fn a_superseded_attempt_cannot_install_its_handles() {
    let _serialised = PRELOAD_TESTS.lock().await;
    let cur_gen = crate::player::current_gen();
    let first = staged("https://cdn.invalid/media/track-a.flac?token=ONE");
    let second = staged("https://cdn.invalid/media/track-b.flac?token=TWO");

    let losing = preload_state().claim(&first, cur_gen).expect("granted");
    let winning = preload_state()
        .claim(&second, cur_gen)
        .expect("a genuinely different next track supersedes it");

    assert!(
        !preload_state().arm_download(losing, CancellationToken::new()),
        "the superseded attempt's token replaced the live one, and the next skip cancelled the wrong download"
    );
    assert!(
        !preload_state().publish(
            losing,
            &first,
            crate::player::buffer::RamBuffer::from_complete(Vec::new())
        ),
        "the superseded attempt published bytes for a track the record no longer names"
    );
    assert!(
        preload_state().arm_download(winning, CancellationToken::new()),
        "the attempt that owns the slot must still be able to write to it"
    );
    preload_state().reset();
}

/// `promote_crossfade` publishes `CURRENT_TRACK` BEFORE flipping the buffer's owner, and this
/// is the premise that makes the order safe rather than merely different.
///
/// The flip is what makes a download restart resolve its url through the retained track instead
/// of the one it captured. Owner first, a restart landing in between reads the OUTGOING track,
/// fails the identity check and abandons the download of what just became audible, silently;
/// it surfaces 30s later as a stall. Track first, the window holds the opposite pair: the
/// track is already current while the buffer still reads `Preload`. That window is inert only
/// because this branch answers from the captured url and never looks at `CURRENT_TRACK`. Should
/// it ever start looking, the promotion order stops being safe and has to be revisited with it.
#[test]
fn a_preload_resolves_its_url_without_the_retained_track() {
    let captured = "https://cdn.invalid/media/track-b.flac?token=CAPTURED&exp=1";

    // A canonical id no retained track can match: consulting `CURRENT_TRACK` here answers None.
    let resolved = DownloadOwner::Preload.fetch_url("no-such-track-is-ever-retained", captured);

    assert_eq!(
        resolved.as_deref(),
        Some(captured),
        "a preload that resolves through CURRENT_TRACK makes the promotion order load-bearing"
    );
}
