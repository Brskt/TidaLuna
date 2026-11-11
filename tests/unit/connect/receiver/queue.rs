//! Tests for `src/connect/receiver/queue/mod.rs`, attached to it by `#[path]`.

use super::*;

fn item(item_id: &str, media_id: &str) -> QueueItem {
    serde_json::from_value(serde_json::json!({
        "item_id": item_id,
        "media_id": media_id,
    }))
    .expect("two ids make a window item")
}

/// A receiver playing `items[idx]`, with the outbox of its effects in the test's hand.
fn playing(items: Vec<QueueItem>, idx: usize) -> (QueueManager, Outbox) {
    let outbox = Outbox::default();
    let mut queue = QueueManager::new(reqwest::Client::new(), outbox.clone());
    queue.queue_items = items;
    queue.current_index = Some(idx);
    (queue, outbox)
}

/// A receiver whose window is loaded, playing `items[idx]` for a controller.
fn casting(items: Vec<QueueItem>, idx: usize) -> (QueueManager, Outbox) {
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
            let (mut queue, notifies) = casting(vec![item("i-1", "111"), item("i-2", "222")], 0);
            queue.state = state;

            assert_eq!(
                queue.handle_command(1, command, payload).await,
                RequestResult::NoQueue,
                "{state:?}: {command}"
            );
            assert!(
                notifies.take().is_empty(),
                "{state:?}: a refused {command} acted anyway"
            );
            assert_eq!(queue.state, state, "{command}");
        }
    }
}

#[tokio::test]
async fn a_queue_whose_session_is_over_answers_as_no_queue_loaded() {
    let (mut queue, _notifies) = casting(vec![item("i-1", "111"), item("i-2", "222")], 0);
    queue.queue_gen = 5;

    queue.end_session();

    assert_eq!(queue.step(QueueStep::Next).await, RequestResult::NoQueue);
    assert_eq!(
        queue.queue_gen, 5,
        "end_session did not carry the queue generation over"
    );
}
