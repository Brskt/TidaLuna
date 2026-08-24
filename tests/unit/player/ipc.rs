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
            product_id: None,
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
            product_id: None,
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
            product_id: None,
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

/// The gapless advance is driven in Rust and no later load re-tags the track, so the preload is
/// the only chance to name it. Untagged, every advanced track played with no duration.
#[test]
fn a_preload_carries_the_track_it_is_for() {
    let parsed = parse_player_ipc(
        "player.preload",
        &[
            json!("https://a"),
            json!("flac"),
            json!("k"),
            json!("261445590"),
        ],
        None,
    )
    .unwrap();
    assert_eq!(
        parsed,
        PlayerIpc::Preload {
            url: "https://a".to_string(),
            format: "flac".to_string(),
            key: "k".to_string(),
            product_id: Some("261445590".to_string()),
        }
    );
}

#[test]
fn a_preload_without_an_id_still_parses() {
    // Three args is the shape every renderer sent before the id existed. It must not become
    // an InvalidArgs, or a stale bundle would stop preloading altogether.
    let parsed = parse_player_ipc(
        "player.preload",
        &[json!("https://a"), json!("flac"), json!("k")],
        None,
    )
    .unwrap();
    assert!(matches!(
        parsed,
        PlayerIpc::Preload {
            product_id: None,
            ..
        }
    ));
}

#[test]
fn a_blank_preload_id_is_read_as_absent() {
    // The renderer sends "" when the queue names nothing after the current item. A blank would
    // compare equal to the next blank one and lend a length across tracks.
    let parsed = parse_player_ipc(
        "player.preload",
        &[json!("https://a"), json!("flac"), json!("k"), json!("  ")],
        None,
    )
    .unwrap();
    assert!(matches!(
        parsed,
        PlayerIpc::Preload {
            product_id: None,
            ..
        }
    ));
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

#[test]
fn a_load_carries_the_track_it_is_for() {
    let parsed = parse_player_ipc(
        "player.load",
        &[
            json!("https://a"),
            json!("flac"),
            json!("k"),
            json!(false),
            json!(false),
            json!("12345"),
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
            product_id: Some("12345".to_string()),
            restart: false,
            want_play: false,
        }
    );
}

#[test]
fn a_dash_load_carries_the_track_it_is_for() {
    let parsed = parse_player_ipc(
        "player.load_dash",
        &[
            json!("https://init"),
            json!("[\"https://s1\"]"),
            json!("aac"),
            json!("777"),
        ],
        None,
    )
    .unwrap();
    assert_eq!(
        parsed,
        PlayerIpc::LoadDash {
            init_url: "https://init".to_string(),
            segment_urls: vec!["https://s1".to_string()],
            format: "aac".to_string(),
            product_id: Some("777".to_string()),
        }
    );
}

#[test]
fn a_blank_track_id_is_read_as_absent() {
    // A blank id would compare equal to another blank one, which is how a length gets lent
    // to a track it was not measured on.
    let parsed = parse_player_ipc(
        "player.load",
        &[
            json!("https://a"),
            json!("flac"),
            json!("k"),
            json!(false),
            json!(false),
            json!("   "),
        ],
        None,
    )
    .unwrap();
    assert!(matches!(
        parsed,
        PlayerIpc::Load {
            product_id: None,
            ..
        }
    ));
}

#[test]
fn a_numeric_track_id_reads_as_the_same_track_as_its_string() {
    // The renderer forwards TIDAL's `mediaProduct.productId` untouched and TIDAL's own ids are
    // numbers, so both spellings reach this parser. Reading only strings tagged those loads
    // with nothing while the metadata frame for the same track stringified it, and `same_track`
    // could then never match: the length stayed withheld for the track's whole life.
    let load_with = |id: serde_json::Value| {
        parse_player_ipc(
            "player.load",
            &[
                json!("https://a"),
                json!("flac"),
                json!("k"),
                json!(false),
                json!(false),
                id,
            ],
            None,
        )
        .unwrap()
    };

    assert_eq!(
        load_with(json!(88264189)),
        load_with(json!("88264189")),
        "the two spellings of one id parsed as two different tracks"
    );
    assert!(
        matches!(
            load_with(json!(88264189)),
            PlayerIpc::Load { product_id: Some(ref id), .. } if id == "88264189"
        ),
        "a numeric id was read as no id at all"
    );
}
