//! Tests for `src/player/ipc.rs`, attached to it by `#[path]`.

use super::{OutputMode, PlayerIpc, parse_player_ipc};
use serde_json::json;

#[test]
fn parses_load_tuple_shape() {
    let parsed = parse_player_ipc(
        "player.load",
        &[json!("https://a"), json!("flac"), json!("k")],
        None,
    )
    .unwrap();
    assert_eq!(
        parsed,
        PlayerIpc::Load {
            url: "https://a".to_string(),
            format: "flac".to_string(),
            key: "k".to_string(),
            restart: false,
            want_play: false,
        }
    );
}

#[test]
fn parses_load_restart_flag() {
    let parsed = parse_player_ipc(
        "player.load",
        &[json!("https://a"), json!("flac"), json!("k"), json!(true)],
        None,
    )
    .unwrap();
    assert_eq!(
        parsed,
        PlayerIpc::Load {
            url: "https://a".to_string(),
            format: "flac".to_string(),
            key: "k".to_string(),
            restart: true,
            want_play: false,
        }
    );
}

#[test]
fn parses_load_want_play_flag() {
    let parsed = parse_player_ipc(
        "player.load",
        &[
            json!("https://a"),
            json!("flac"),
            json!("k"),
            json!(false),
            json!(true),
        ],
        None,
    )
    .unwrap();
    assert_eq!(
        parsed,
        PlayerIpc::Load {
            url: "https://a".to_string(),
            format: "flac".to_string(),
            key: "k".to_string(),
            restart: false,
            want_play: true,
        }
    );
}

#[test]
fn parses_recover_object_shape() {
    let parsed = parse_player_ipc(
        "player.recover",
        &[json!({
            "url": "https://a",
            "streamFormat": "flac",
            "encryptionKey": "k",
            "currentTime": 12.5
        })],
        None,
    )
    .unwrap();
    assert_eq!(
        parsed,
        PlayerIpc::Recover {
            url: "https://a".to_string(),
            format: "flac".to_string(),
            key: "k".to_string(),
            target_time: Some(12.5),
        }
    );
}

#[test]
fn parses_recover_positional_shape() {
    let parsed = parse_player_ipc(
        "player.recover",
        &[json!("https://a"), json!("aac"), json!("k"), json!(33.0)],
        None,
    )
    .unwrap();
    assert_eq!(
        parsed,
        PlayerIpc::Recover {
            url: "https://a".to_string(),
            format: "aac".to_string(),
            key: "k".to_string(),
            target_time: Some(33.0),
        }
    );
}

#[test]
fn parses_seek_and_volume_numeric_shapes() {
    assert_eq!(
        parse_player_ipc("player.seek", &[json!(17.25)], None).unwrap(),
        PlayerIpc::Seek { time: 17.25 }
    );
    assert_eq!(
        parse_player_ipc("player.volume", &[json!(65.0)], None).unwrap(),
        PlayerIpc::Volume { volume: 65.0 }
    );
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

#[test]
fn errors_on_invalid_required_args() {
    assert!(parse_player_ipc("player.load", &[json!("https://a")], None).is_err());
    assert!(parse_player_ipc("player.metadata", &[], None).is_err());
}
