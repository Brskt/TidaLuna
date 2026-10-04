//! Tests for `src/connect/receiver/queue/mod.rs`, attached to it by `#[path]`.

use super::*;

fn tok(access: &str, status: GenerationStatus) -> TokenState {
    TokenState {
        generation: 1,
        token_version: 1,
        access_token: access.to_string(),
        refresh_token: Some("rt".to_string()),
        scope: Some("read".to_string()),
        expires_at: Instant::now() + Duration::from_secs(3600),
        status,
    }
}

// ── Adoption of a locally promoted track ─────────────────────────────

/// A window item carrying no stream url: an adoption never resolves one, which is what lets it
/// finish inside a single turn of the routing loop.
fn item(item_id: &str, media_id: &str) -> QueueItem {
    serde_json::from_value(serde_json::json!({
        "item_id": item_id,
        "media_id": media_id,
    }))
    .expect("two ids are all an adoption reads")
}

/// A receiver playing `items[idx]`, with the notify channel in the test's hand.
fn playing(items: Vec<QueueItem>, idx: usize) -> (QueueManager, mpsc::Receiver<QueueNotifyEvent>) {
    let (tx, rx) = mpsc::channel(8);
    let mut queue = QueueManager::new(reqwest::Client::new(), tx);
    queue.queue_items = items;
    queue.current_index = Some(idx);
    (queue, rx)
}

#[tokio::test]
async fn the_promotion_names_the_track_adopted_not_the_index() {
    // The fade promotes off the renderer's own staged next; this window comes from the cloud
    // queue. Nothing couples the two registries; `idx + 1` names the promoted track only by
    // luck, and when it does not, another track's title and artwork go to the phone as "now
    // playing", with the index moved to match the fiction.
    let (mut queue, _notifies) = playing(
        vec![item("i-1", "111"), item("i-2", "222"), item("i-3", "333")],
        0,
    );

    let (media, _seq) = queue
        .adopt_promoted(Some("333"))
        .expect("an adoption to hand to the controller");

    assert_eq!(
        media.media_id, "333",
        "the receiver adopted the track its index guessed, not the one actually promoted"
    );
    assert_eq!(queue.current_index, Some(2));
}

#[tokio::test]
async fn a_next_after_a_promotion_steps_past_the_track_now_playing() {
    // The defect this shape exists to close. The adoption used to travel two more hops, and a
    // `next` from the phone was served while the index still named the OUTGOING track: it
    // stepped to the track the fade had already made audible and restarted it. The listener
    // could hear B and asked for the one after it.
    let (mut queue, _notifies) = playing(
        vec![item("i-1", "111"), item("i-2", "222"), item("i-3", "333")],
        0,
    );

    // Same turn of the loop: the promotion is taken before anything else can be served.
    queue.adopt_promoted(Some("222"));
    queue.skip_next().await;

    assert_eq!(
        queue.current_index,
        Some(2),
        "next stepped back onto the track the crossfade had already started"
    );
}

#[tokio::test]
async fn a_promoted_track_outside_the_queue_is_not_announced() {
    // The speaker left the controller's queue entirely. No index is correct: naming any
    // item is a positive lie, worse than silence, which at least leaves the last true
    // announcement standing.
    let (mut queue, _notifies) = playing(vec![item("i-1", "111"), item("i-2", "222")], 0);

    assert!(
        queue.adopt_promoted(Some("999")).is_none(),
        "a track absent from the queue was handed to the controller anyway"
    );
    assert_eq!(
        queue.current_index, None,
        "the receiver went on claiming a position it cannot know"
    );
}

#[tokio::test]
async fn a_promotion_agreeing_with_the_index_adopts_it() {
    // The common path, and the one that must not change: the two registries agree and the
    // arithmetic was right all along; it is now proven right rather than assumed.
    let (mut queue, _notifies) = playing(vec![item("i-1", "111"), item("i-2", "222")], 0);

    let (media, _seq) = queue
        .adopt_promoted(Some("222"))
        .expect("an adoption to hand to the controller");

    assert_eq!(media.media_id, "222");
    assert_eq!(queue.current_index, Some(1));
}

#[tokio::test]
async fn a_queue_holding_a_track_twice_adopts_the_copy_the_step_names() {
    // `media_id` is not unique: the same track can sit at two positions, which is why the
    // receiver's own window resolution tries `item_id` before it. Searched blind, the promoted
    // id would resolve to the earlier copy and walk the index BACKWARDS over a queue that only
    // moved forward.
    let (mut queue, _notifies) = playing(
        vec![item("i-1", "111"), item("i-2", "222"), item("i-3", "111")],
        1,
    );

    queue.adopt_promoted(Some("111"));

    assert_eq!(
        queue.current_index,
        Some(2),
        "the adoption walked back to an earlier copy of the same track"
    );
}

#[tokio::test]
async fn an_unnamed_promotion_still_advances_by_one() {
    // A preload staged without an id yields a promotion that names nothing. That is not new
    // information to act on. The arithmetic stands: going inert here would lose ground
    // against the behaviour this replaces, for a case no id can improve.
    let (mut queue, _notifies) = playing(vec![item("i-1", "111"), item("i-2", "222")], 0);

    let (media, _seq) = queue
        .adopt_promoted(None)
        .expect("an adoption to hand to the controller");

    assert_eq!(media.media_id, "222");
    assert_eq!(queue.current_index, Some(1));
}

#[test]
fn won_cas_uses_our_token() {
    // We won the CAS: the store already holds our freshly-minted token.
    let current = tok("at-new", GenerationStatus::Active);
    let outcome = reconcile_refresh(Ok(()), "at-new", &current);
    assert_eq!(outcome, RefreshOutcome::UseToken("at-new".to_string()));
}

#[test]
fn lost_cas_with_active_winner_adopts_winner_token() {
    // The audit bug: a benign VersionMismatch must NOT surface as an error
    // and must NOT replay our discarded token. A concurrent writer already
    // installed a current Active token; adopt the WINNER's token.
    let current = tok("at-winner", GenerationStatus::Active);
    let outcome = reconcile_refresh(Err(CASError::VersionMismatch), "at-ours", &current);
    assert_eq!(outcome, RefreshOutcome::UseToken("at-winner".to_string()));
}

#[test]
fn lost_cas_with_terminated_winner_is_terminal() {
    // VersionMismatch where the winner terminated the generation: relogin.
    let current = tok(
        "at-x",
        GenerationStatus::Terminated(TerminationReason::InvalidGrant {
            provider_error: "invalid_grant".to_string(),
            suspect_replay: false,
        }),
    );
    let outcome = reconcile_refresh(Err(CASError::VersionMismatch), "at-ours", &current);
    assert_eq!(
        outcome,
        RefreshOutcome::Terminal("invalid_grant".to_string())
    );
}

#[test]
fn terminated_snapshot_is_terminal() {
    // Our snapshot's generation was already terminated at CAS time and the
    // store is still terminated: route to relogin, never InvalidResponse.
    let current = tok(
        "at-x",
        GenerationStatus::Terminated(TerminationReason::Revoked),
    );
    let outcome = reconcile_refresh(Err(CASError::Terminated), "at-ours", &current);
    assert_eq!(outcome, RefreshOutcome::Terminal("revoked".to_string()));
}

#[test]
fn terminated_snapshot_but_relogin_landed_adopts_fresh() {
    // Snapshot was terminated, but a wire relogin installed a fresh Active
    // generation while we were refreshing: adopt the fresh token.
    let current = tok("at-fresh", GenerationStatus::Active);
    let outcome = reconcile_refresh(Err(CASError::Terminated), "at-ours", &current);
    assert_eq!(outcome, RefreshOutcome::UseToken("at-fresh".to_string()));
}

/// A receiver whose window is loaded, playing `items[idx]` for a controller.
fn casting(items: Vec<QueueItem>, idx: usize) -> (QueueManager, mpsc::Receiver<QueueNotifyEvent>) {
    let (mut queue, notifies) = playing(items, idx);
    queue.state = QueueState::Loaded;
    (queue, notifies)
}

#[tokio::test]
async fn commands_on_the_queue_are_refused_with_no_queue_loaded() {
    // TIDAL's receiver answers each of these 1002 in its IDLE state, and acts on none.
    let commands = [
        ("next", serde_json::json!({})),
        ("previous", serde_json::json!({})),
        (
            "selectQueueItem",
            serde_json::json!({ "mediaInfo": { "itemId": "i-2", "mediaId": "222" } }),
        ),
        ("setRepeatMode", serde_json::json!({ "repeatMode": "ALL" })),
        ("setShuffle", serde_json::json!({ "shuffle": true })),
    ];
    for state in [QueueState::Idle, QueueState::Loading] {
        for (command, payload) in &commands {
            let (mut queue, mut notifies) =
                casting(vec![item("i-1", "111"), item("i-2", "222")], 0);
            queue.state = state;

            assert_eq!(
                queue.handle_command(1, command, payload).await,
                RequestResult::NoQueue,
                "{state:?}: {command}"
            );
            assert!(
                notifies.try_recv().is_err(),
                "{state:?}: a refused {command} acted anyway"
            );
            assert_eq!(queue.state, state, "{command}");
        }
    }
}

#[tokio::test]
async fn volume_and_mute_are_carried_out_with_no_queue_loaded() {
    // Refused before a queue loads, volume and mute would leave the phone unable to set the
    // level of a cast it has just started.
    for state in [QueueState::Idle, QueueState::Loading] {
        let (mut queue, mut notifies) = casting(Vec::new(), 0);
        queue.state = state;

        assert_eq!(
            queue
                .handle_command(1, "setVolume", &serde_json::json!({ "level": 0.5 }))
                .await,
            RequestResult::Done,
            "{state:?}"
        );
        assert!(matches!(
            notifies.try_recv(),
            Ok(QueueNotifyEvent::SetVolume { level }) if level == 0.5
        ));

        assert_eq!(
            queue
                .handle_command(1, "setMute", &serde_json::json!({ "mute": true }))
                .await,
            RequestResult::Done,
            "{state:?}"
        );
        assert!(matches!(
            notifies.try_recv(),
            Ok(QueueNotifyEvent::SetMute { mute: true })
        ));
    }
}

#[test]
fn the_answer_is_the_message_the_controller_already_reads() {
    for (result, code) in [(RequestResult::Done, 0), (RequestResult::NoQueue, 1002)] {
        assert_eq!(
            serde_json::to_value(result.answer(7)).expect("a plain wire message"),
            serde_json::json!({
                "command": "notifyRequestResult",
                "requestId": 7,
                "resultCode": code,
            })
        );
    }
}
