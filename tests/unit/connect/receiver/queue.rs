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

/// A lost CAS is not an error and does not replay our discarded token: the winner's current
/// token is adopted, and a terminated store routes to relogin, unless a wire relogin landed a
/// fresh Active generation while we were refreshing.
#[test]
fn a_refresh_is_reconciled_with_the_store_the_cas_left_behind() {
    for (row, cas_result, current, expected) in [
        (
            "won the CAS",
            Ok(()),
            tok("at-new", GenerationStatus::Active),
            RefreshOutcome::UseToken("at-ours".to_string()),
        ),
        (
            "lost to an Active winner",
            Err(CASError::VersionMismatch),
            tok("at-winner", GenerationStatus::Active),
            RefreshOutcome::UseToken("at-winner".to_string()),
        ),
        (
            "lost to a winner that terminated the generation",
            Err(CASError::VersionMismatch),
            tok(
                "at-x",
                GenerationStatus::Terminated(TerminationReason::InvalidGrant {
                    provider_error: "invalid_grant".to_string(),
                    suspect_replay: false,
                }),
            ),
            RefreshOutcome::Terminal("invalid_grant".to_string()),
        ),
        (
            "snapshot terminated, store still terminated",
            Err(CASError::Terminated),
            tok(
                "at-x",
                GenerationStatus::Terminated(TerminationReason::Revoked),
            ),
            RefreshOutcome::Terminal("revoked".to_string()),
        ),
        (
            "snapshot terminated, a relogin landed",
            Err(CASError::Terminated),
            tok("at-fresh", GenerationStatus::Active),
            RefreshOutcome::UseToken("at-fresh".to_string()),
        ),
    ] {
        assert_eq!(
            reconcile_refresh(cas_result, "at-ours", &current),
            expected,
            "{row}"
        );
    }
}

/// A receiver whose window is loaded, playing `items[idx]` for a controller.
fn casting(items: Vec<QueueItem>, idx: usize) -> (QueueManager, mpsc::Receiver<QueueNotifyEvent>) {
    let (mut queue, notifies) = playing(items, idx);
    queue.state = QueueState::Loaded;
    (queue, notifies)
}

const PICK: Option<ListenerAct> = Some(ListenerAct::Pick);
const AUTOMATIC: Option<ListenerAct> = None;

/// TIDAL's advance on the SDK's `ended` names a track the queue may hold, and its Next reloads
/// the cast's own: settled as playing, that reload's autoplay resumed a cast the phone had
/// paused. A step plays whatever track the page names, and a load not owned by Connect is refused.
#[test]
fn a_load_on_the_first_item_is_routed_by_its_act_and_owner() {
    let (queue, _notifies) = casting(vec![item("i-1", "111"), item("i-2", "222")], 0);

    for (media_id, act, origin, expected) in [
        (
            "222",
            PICK,
            LoadOrigin::Connect,
            CastRoute::Select { index: 1 },
        ),
        ("999", PICK, LoadOrigin::Connect, CastRoute::RefusePick),
        ("222", AUTOMATIC, LoadOrigin::Connect, CastRoute::Refuse),
        ("111", NEXT, LoadOrigin::Connect, CastRoute::Next),
        ("999", NEXT, LoadOrigin::Connect, CastRoute::Next),
        ("111", PREVIOUS, LoadOrigin::Connect, CastRoute::Previous),
        ("111", NEXT, LoadOrigin::Local, CastRoute::Refuse),
        ("111", PREVIOUS, LoadOrigin::Local, CastRoute::Refuse),
    ] {
        assert_eq!(
            queue.cast_route(media_id, act, origin),
            expected,
            "{media_id}, {act:?}, {origin:?}"
        );
    }
}

/// TIDAL reloads its playing track on its own (a pick played past its length, a page reload's
/// restore): refusing it raised the "controlled via another client" banner. A pick of it is a
/// select, never a settle, and a load served after the cast ended has nothing to settle on.
#[test]
fn a_load_naming_the_track_the_cast_plays_is_routed_by_its_act_and_owner() {
    let (queue, _notifies) = casting(vec![item("i-1", "111"), item("i-2", "222")], 1);

    for (act, origin, expected) in [
        (AUTOMATIC, LoadOrigin::Connect, CastRoute::Playing),
        (PICK, LoadOrigin::Connect, CastRoute::Select { index: 1 }),
        (PICK, LoadOrigin::Local, CastRoute::Refuse),
        (AUTOMATIC, LoadOrigin::Local, CastRoute::Refuse),
    ] {
        assert_eq!(
            queue.cast_route("222", act, origin),
            expected,
            "{act:?}, {origin:?}"
        );
    }
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
