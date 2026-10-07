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

fn item(item_id: &str, media_id: &str) -> QueueItem {
    serde_json::from_value(serde_json::json!({
        "item_id": item_id,
        "media_id": media_id,
    }))
    .expect("two ids make a window item")
}

/// A receiver playing `items[idx]`, with the notify channel in the test's hand.
fn playing(items: Vec<QueueItem>, idx: usize) -> (QueueManager, mpsc::Receiver<QueueNotifyEvent>) {
    let (tx, rx) = mpsc::channel(8);
    let mut queue = QueueManager::new(reqwest::Client::new(), tx);
    queue.queue_items = items;
    queue.current_index = Some(idx);
    (queue, rx)
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

const PICK: Option<ListenerAct> = Some(ListenerAct::Pick);
const AUTOMATIC: Option<ListenerAct> = None;

#[test]
fn a_pick_in_the_controllers_queue_selects_that_item() {
    let (queue, _notifies) = casting(vec![item("i-1", "111"), item("i-2", "222")], 0);

    assert_eq!(
        queue.cast_route("222", PICK, LoadOrigin::Connect),
        CastRoute::Select { index: 1 }
    );
}

#[test]
fn a_pick_outside_the_controllers_queue_is_refused() {
    let (queue, _notifies) = casting(vec![item("i-1", "111"), item("i-2", "222")], 0);

    assert_eq!(
        queue.cast_route("999", PICK, LoadOrigin::Connect),
        CastRoute::RefusePick
    );
}

#[test]
fn an_automatic_load_never_changes_the_track_even_one_the_queue_holds() {
    // TIDAL's advance on the SDK's `ended` names the page's next track, which the controller's
    // queue may well hold.
    let (queue, _notifies) = casting(vec![item("i-1", "111"), item("i-2", "222")], 0);

    assert_eq!(
        queue.cast_route("222", AUTOMATIC, LoadOrigin::Connect),
        CastRoute::Refuse
    );
}

#[test]
fn an_automatic_load_of_the_track_the_cast_plays_is_settled_as_playing() {
    // TIDAL reloads its playing track on its own (a pick played past its length, a page reload's
    // restore); refusing it raised the "controlled via another client" banner over it.
    let (queue, _notifies) = casting(vec![item("i-1", "111"), item("i-2", "222")], 1);

    assert_eq!(
        queue.cast_route("222", AUTOMATIC, LoadOrigin::Connect),
        CastRoute::Playing
    );
}

#[test]
fn a_pick_of_the_track_the_cast_plays_restarts_it() {
    // A controller selecting its current item reloads it too: a pick is a select, never a settle.
    let (queue, _notifies) = casting(vec![item("i-1", "111"), item("i-2", "222")], 1);

    assert_eq!(
        queue.cast_route("222", PICK, LoadOrigin::Connect),
        CastRoute::Select { index: 1 }
    );
}

#[test]
fn a_load_served_after_the_cast_ended_is_refused() {
    // The session ended between the load and its turn in the loop: nothing plays for it to
    // settle on.
    let (queue, _notifies) = casting(vec![item("i-1", "111"), item("i-2", "222")], 1);

    assert_eq!(
        queue.cast_route("222", PICK, LoadOrigin::Local),
        CastRoute::Refuse
    );
    assert_eq!(
        queue.cast_route("222", AUTOMATIC, LoadOrigin::Local),
        CastRoute::Refuse
    );
}

#[test]
fn a_load_while_the_window_is_not_loaded_is_refused() {
    // Without a loaded window the items go unread, even to settle on the track the cast plays.
    let (mut queue, _notifies) = casting(vec![item("i-1", "111"), item("i-2", "222")], 1);
    queue.state = QueueState::Loading;

    assert_eq!(
        queue.cast_route("222", AUTOMATIC, LoadOrigin::Connect),
        CastRoute::Refuse
    );
}

#[test]
fn a_pick_while_the_window_is_not_loaded_is_refused_as_a_pick() {
    // Refused as any other load, it left the page's queue on the pick, which a Play after the cast
    // resumed: only the refusal of a pick announces the cast's track back to the page.
    let (mut queue, _notifies) = casting(vec![item("i-1", "111"), item("i-2", "222")], 1);

    for state in [QueueState::Idle, QueueState::Loading, QueueState::Moving] {
        queue.state = state;
        for media_id in ["222", "999"] {
            assert_eq!(
                queue.cast_route(media_id, PICK, LoadOrigin::Connect),
                CastRoute::RefusePick,
                "{state:?}, pick {media_id}"
            );
        }
    }
}

#[test]
fn a_track_queued_twice_resolves_to_the_copy_the_controller_reaches_next() {
    // The earlier copy is one the controller has already played past.
    let (queue, _notifies) = casting(
        vec![item("i-1", "111"), item("i-2", "222"), item("i-3", "111")],
        1,
    );

    assert_eq!(
        queue.cast_route("111", PICK, LoadOrigin::Connect),
        CastRoute::Select { index: 2 }
    );
}

#[test]
fn every_route_answers_the_load_it_was_asked_for() {
    for (route, answer) in [
        (CastRoute::Select { index: 0 }, "mediacastrouted"),
        (CastRoute::Playing, "mediacastrouted"),
        (CastRoute::Next, "mediacastowned"),
        (CastRoute::Previous, "mediacastowned"),
        (CastRoute::RefusePick, "mediacastowned"),
        (CastRoute::Refuse, "mediacastowned"),
    ] {
        let sent = serde_json::to_value(route.answer(7)).expect("a plain wire message");
        assert_eq!(sent["t"], answer, "{route:?}");
        assert_eq!(sent["v"]["load"], 7, "{route:?} names another load");
    }
}

#[tokio::test]
async fn a_selected_pick_is_played_as_the_controllers_own_select() {
    // Stops at the `SetMedia`: its load, not `select_pick`, sends the `notifyMediaChanged`.
    let (mut queue, mut notifies) = casting(vec![item("i-1", "111"), item("i-2", "222")], 0);

    queue.select_pick(1).await;

    assert_eq!(queue.current_index, Some(1));
    assert_eq!(queue.state, QueueState::Loaded);
    let mut set_media = None;
    while let Ok(event) = notifies.try_recv() {
        if let QueueNotifyEvent::SetMedia { media_info, .. } = event {
            set_media = Some(media_info);
        }
    }
    let media = set_media.expect("the picked item handed to playback");
    assert_eq!(
        (media.item_id.as_str(), media.media_id.as_str()),
        ("i-2", "222")
    );
}

const NEXT: Option<ListenerAct> = Some(ListenerAct::Next);
const PREVIOUS: Option<ListenerAct> = Some(ListenerAct::Previous);

#[test]
fn a_listeners_next_steps_the_cast_whatever_track_the_page_reloads() {
    // The page's queue holds the cast's track alone, so TIDAL's Next reloads that same track.
    // Settled as playing, the reload's autoplay resumed a cast the phone had paused.
    let (queue, _notifies) = casting(vec![item("i-1", "111"), item("i-2", "222")], 0);

    assert_eq!(
        queue.cast_route("111", NEXT, LoadOrigin::Connect),
        CastRoute::Next
    );
    assert_eq!(
        queue.cast_route("999", NEXT, LoadOrigin::Connect),
        CastRoute::Next
    );
    assert_eq!(
        queue.cast_route("111", PREVIOUS, LoadOrigin::Connect),
        CastRoute::Previous
    );
}

#[test]
fn a_listeners_step_with_no_cast_to_step_is_refused() {
    let (queue, _notifies) = casting(vec![item("i-1", "111"), item("i-2", "222")], 0);

    assert_eq!(
        queue.cast_route("111", NEXT, LoadOrigin::Local),
        CastRoute::Refuse
    );
    assert_eq!(
        queue.cast_route("111", PREVIOUS, LoadOrigin::Local),
        CastRoute::Refuse
    );
}

#[test]
fn a_listeners_step_reaches_the_queue_whatever_state_its_window_is_in() {
    // Refused by `cast_route`, the listener's step never reached `step`.
    let (mut queue, _notifies) = casting(vec![item("i-1", "111"), item("i-2", "222")], 0);

    for state in [QueueState::Idle, QueueState::Loading, QueueState::Moving] {
        queue.state = state;
        assert_eq!(
            queue.cast_route("111", NEXT, LoadOrigin::Connect),
            CastRoute::Next,
            "{state:?}"
        );
        assert_eq!(
            queue.cast_route("111", PREVIOUS, LoadOrigin::Connect),
            CastRoute::Previous,
            "{state:?}"
        );
    }
}

/// A window response naming `items`, as the queue server sends it.
fn window(items: &[(&str, &str)]) -> serde_json::Value {
    serde_json::json!({
        "items": items
            .iter()
            .map(|(item_id, media_id)| serde_json::json!({ "item_id": item_id, "media_id": media_id }))
            .collect::<Vec<_>>(),
    })
}

#[tokio::test]
async fn a_step_with_no_queue_loaded_is_refused_for_the_listener_and_the_controller_alike() {
    // One gate for the listener and the controller (see `handle_command`): a step kept back would
    // play after whichever window landed next, in any session, or never.
    for state in [QueueState::Idle, QueueState::Loading] {
        for listener in [true, false] {
            let (mut queue, mut notifies) =
                casting(vec![item("i-1", "111"), item("i-2", "222")], 0);
            queue.current_item_id_hint = Some("i-1".to_string());
            queue.state = state;

            let result = if listener {
                queue.step(QueueStep::Next).await
            } else {
                queue
                    .handle_command(1, "next", &serde_json::json!({}))
                    .await
            };
            assert_eq!(
                result,
                RequestResult::NoQueue,
                "{state:?}, listener: {listener}"
            );

            queue
                .process_queue_window_response(window(&[("i-1", "111"), ("i-2", "222")]))
                .await;

            assert!(
                handed_to_playback(&mut notifies).is_empty(),
                "a refused step played once a window landed ({state:?}, listener: {listener})"
            );
            assert_eq!(queue.current_index, Some(0));
        }
    }
}

#[tokio::test]
async fn a_listeners_step_in_a_loaded_window_plays_at_once() {
    let (mut queue, mut notifies) = casting(vec![item("i-1", "111"), item("i-2", "222")], 1);

    assert_eq!(queue.step(QueueStep::Previous).await, RequestResult::Done);

    assert_eq!(
        handed_to_playback(&mut notifies),
        vec![("i-1".to_string(), "111".to_string())]
    );
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

/// The media of every `SetMedia` the receiver handed to playback, in order.
fn handed_to_playback(notifies: &mut mpsc::Receiver<QueueNotifyEvent>) -> Vec<(String, String)> {
    let mut handed = Vec::new();
    while let Ok(event) = notifies.try_recv() {
        if let QueueNotifyEvent::SetMedia { media_info, .. } = event {
            handed.push((media_info.item_id, media_info.media_id));
        }
    }
    handed
}

#[tokio::test]
async fn a_listeners_next_plays_the_controllers_next_item() {
    let (mut queue, mut notifies) = casting(vec![item("i-1", "111"), item("i-2", "222")], 0);

    queue.skip_next().await;

    assert_eq!(queue.current_index, Some(1));
    assert_eq!(
        handed_to_playback(&mut notifies),
        vec![("i-2".to_string(), "222".to_string())]
    );
}

#[tokio::test]
async fn a_listeners_next_at_the_queues_end_hands_nothing_to_playback() {
    // As the controller's own `next` there: nothing plays, and a paused cast stays paused.
    let (mut queue, mut notifies) = casting(vec![item("i-1", "111"), item("i-2", "222")], 1);

    queue.skip_next().await;

    assert_eq!(queue.current_index, Some(1));
    assert!(handed_to_playback(&mut notifies).is_empty());
}

#[tokio::test]
async fn a_queue_whose_session_is_over_answers_as_no_queue_loaded() {
    let (mut queue, mut notifies) = casting(vec![item("i-1", "111"), item("i-2", "222")], 0);
    queue.content_server = Some(
        serde_json::from_value(serde_json::json!({ "serverUrl": "https://api.tidal.com/v1" }))
            .expect("a url makes a server"),
    );
    queue.queue_server = queue.content_server.clone();
    queue.current_media_id_hint = Some("111".to_string());
    queue.current_item_id_hint = Some("i-1".to_string());
    queue.queue_gen = 5;

    queue.end_session();

    assert_eq!(queue.step(QueueStep::Next).await, RequestResult::NoQueue);
    assert_eq!(
        queue.cast_route("222", PICK, LoadOrigin::Connect),
        CastRoute::RefusePick
    );
    queue.on_media_ended().await;
    assert!(handed_to_playback(&mut notifies).is_empty());
    assert!(queue.content_server.is_none() && queue.queue_server.is_none());
    assert!(queue.current_media_id_hint.is_none() && queue.current_item_id_hint.is_none());
    assert_eq!(
        queue.queue_gen, 5,
        "end_session did not carry the queue generation over"
    );
}
