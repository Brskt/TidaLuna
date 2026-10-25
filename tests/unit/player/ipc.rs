//! Tests for `src/player/ipc.rs`, attached to it by `#[path]`.

use super::{
    ListenerAct, OutputMode, PlayerIpc, PlayerIpcParseError, SegmentUrls, parse_player_ipc,
};
use serde_json::json;

#[test]
fn parses_recover_object_and_positional_shapes() {
    for (args, expected) in [
        (
            vec![json!({
                "url": "https://a",
                "streamFormat": "flac",
                "encryptionKey": "k",
                "currentTime": 12.5
            })],
            PlayerIpc::Recover {
                url: "https://a".to_string(),
                format: "flac".to_string(),
                key: "k".to_string(),
                target_time: Some(12.5),
            },
        ),
        (
            vec![json!("https://a"), json!("aac"), json!("k"), json!(33.0)],
            PlayerIpc::Recover {
                url: "https://a".to_string(),
                format: "aac".to_string(),
                key: "k".to_string(),
                target_time: Some(33.0),
            },
        ),
    ] {
        assert_eq!(
            parse_player_ipc("player.recover", &args, None).unwrap(),
            expected,
            "{args:?}"
        );
    }
}

/// With the gapless advance driven in Rust and no later load re-tagging the track, the preload is
/// the only chance to name it. The 3-arg shape of older renderers still parses, and a blank id (the
/// queue names nothing next) is absent: two blanks compare equal and would lend a length across tracks.
#[test]
fn a_preload_carries_the_track_it_is_for() {
    let head = [json!("https://a"), json!("flac"), json!("k")];
    for (id, expected) in [
        (Some(json!("261445590")), Some("261445590")),
        (None, None),
        (Some(json!("  ")), None),
    ] {
        let args: Vec<_> = head.iter().cloned().chain(id).collect();
        assert_eq!(
            parse_player_ipc("player.preload", &args, None).unwrap(),
            PlayerIpc::Preload {
                url: "https://a".to_string(),
                format: "flac".to_string(),
                key: "k".to_string(),
                product_id: expected.map(ToString::to_string),
            },
            "{args:?}"
        );
    }
}

#[test]
fn parses_devices_set_shape() {
    assert_eq!(
        parse_player_ipc(
            "player.devices.set",
            &[json!("id-1"), json!("exclusive")],
            None
        )
        .unwrap(),
        PlayerIpc::DevicesSet {
            id: "id-1".to_string(),
            mode: OutputMode::Exclusive,
        }
    );
}

/// The refusal is the only door to a DASH load: a list that is empty, unreadable or missing, or
/// an init url that is blank, never becomes a command.
#[test]
fn a_dash_load_carries_its_track_and_is_refused_without_a_segment_or_an_init_url() {
    let refused = || Err(PlayerIpcParseError::InvalidArgs("player.load_dash"));
    for (args, expected) in [
        (
            vec![
                json!("https://init"),
                json!("[\"https://s1\"]"),
                json!("aac"),
                json!("777"),
            ],
            Ok(PlayerIpc::LoadDash {
                init_url: "https://init".to_string(),
                segment_urls: SegmentUrls::new(vec!["https://s1".to_string()]).unwrap(),
                format: "aac".to_string(),
                product_id: Some("777".to_string()),
            }),
        ),
        (
            vec![json!("https://init"), json!("[]"), json!("aac")],
            refused(),
        ),
        (
            vec![json!("https://init"), json!("not json"), json!("aac")],
            refused(),
        ),
        (vec![json!("https://init")], refused()),
        (
            vec![json!(""), json!("[\"https://s1\"]"), json!("aac")],
            refused(),
        ),
    ] {
        assert_eq!(
            parse_player_ipc("player.load_dash", &args, None),
            expected,
            "{args:?}"
        );
    }
    assert_eq!(SegmentUrls::new(Vec::new()), None);
}

/// A blank id reads as absent (two blanks compare equal and lend a length across tracks), and
/// TIDAL's numeric `productId` reads as its string: reading only strings tagged those loads with
/// nothing, `same_track` never matched, and the length stayed withheld for the track's whole life.
#[test]
fn a_track_id_reads_as_the_same_track_whatever_its_spelling() {
    for (id, expected) in [
        (json!("   "), None),
        (json!(88264189), Some("88264189")),
        (json!("88264189"), Some("88264189")),
    ] {
        let parsed = parse_player_ipc(
            "player.load",
            &[
                json!("https://a"),
                json!("flac"),
                json!("k"),
                json!(false),
                json!(false),
                id.clone(),
            ],
            None,
        )
        .unwrap();
        let product_id = match parsed {
            PlayerIpc::Load { product_id, .. } => product_id,
            other => panic!("{other:?}"),
        };
        assert_eq!(product_id.as_deref(), expected, "{id:?}");
    }
}

/// A delegate load carries its number and the listener's act it answers. An automatic load, and
/// anything the renderer does not name, is no act.
#[test]
fn a_delegate_load_carries_its_number_and_names_the_listeners_act_it_answers() {
    for (flags, wire, expected) in [
        (true, json!("pick"), Some(ListenerAct::Pick)),
        (false, json!("next"), Some(ListenerAct::Next)),
        (false, json!("previous"), Some(ListenerAct::Previous)),
        (false, json!(""), None),
        (false, json!(true), None),
    ] {
        let parsed = parse_player_ipc(
            "player.load",
            &[
                json!("https://a"),
                json!("flac"),
                json!("k"),
                json!(flags),
                json!(flags),
                json!("12345"),
                json!(9),
                wire.clone(),
            ],
            None,
        )
        .unwrap();
        let (load_id, act) = match parsed {
            PlayerIpc::Load { load_id, act, .. } => (load_id, act),
            other => panic!("{other:?}"),
        };
        assert_eq!(load_id, Some(9), "{wire:?}");
        assert_eq!(act, expected, "{wire:?}");
    }
}
