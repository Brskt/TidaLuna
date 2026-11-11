//! Tests for `src/player/ipc.rs`, attached to it by `#[path]`.

use super::{
    CastGate, CastLoad, ListenerAct, OutputMode, PlayerIpc, PlayerIpcParseError, SegmentUrls,
    parse_player_ipc,
};
use crate::player::LoadOrigin;
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

// Every renderer path that cut a cast ended on one of these channels: this page's auto-advance
// and self-load, TIDAL's queue sagas, a page reload's cold restore, the SDK's reset before each
// load. Pinned on the command type: the handler that applies the rule needs the whole `AppState`.

fn load() -> PlayerIpc {
    PlayerIpc::Load {
        url: "https://cdn/mediatracks/local.mp4?token=SECRET".to_string(),
        format: "mp4a".to_string(),
        key: String::new(),
        product_id: Some("2".to_string()),
        restart: false,
        want_play: true,
        load_id: None,
        act: None,
    }
}

fn load_dash() -> PlayerIpc {
    PlayerIpc::LoadDash {
        init_url: "https://cdn/init.mp4".to_string(),
        segment_urls: SegmentUrls::new(vec!["https://cdn/1.mp4".to_string()]).unwrap(),
        format: "mp4a".to_string(),
        product_id: Some("2".to_string()),
    }
}

fn recover() -> PlayerIpc {
    PlayerIpc::Recover {
        url: "https://cdn/mediatracks/local.flac".to_string(),
        format: "flac".to_string(),
        key: String::new(),
        target_time: Some(12.0),
    }
}

/// The commands a cast holds back.
fn takeovers() -> Vec<PlayerIpc> {
    vec![
        load(),
        load_dash(),
        recover(),
        PlayerIpc::Stop,
        PlayerIpc::Metadata {
            payload: json!({ "productId": "2", "title": "Local pick" }),
        },
    ]
}

/// Every other command the renderer sends: none replaces the track.
fn passthroughs() -> Vec<PlayerIpc> {
    vec![
        PlayerIpc::Preload {
            url: "https://cdn/next.flac".to_string(),
            format: "flac".to_string(),
            key: String::new(),
            product_id: Some("3".to_string()),
        },
        PlayerIpc::PreloadCancel,
        PlayerIpc::Play,
        PlayerIpc::Pause,
        PlayerIpc::Seek { time: 30.0 },
        PlayerIpc::Volume {
            volume: 40.0,
            seen: Some(1),
        },
        PlayerIpc::DevicesGet { request_id: None },
        PlayerIpc::DevicesSet {
            id: "auto".to_string(),
            mode: OutputMode::Shared,
        },
    ]
}

fn every_command() -> Vec<PlayerIpc> {
    takeovers().into_iter().chain(passthroughs()).collect()
}

/// A cast refuses the loads the renderer asks for, routes a numbered one to the receiver with its
/// act (the automatic one too: only the receiver knows whether the cast already plays it), and
/// drops the stop the SDK sends ahead of every load, and the announcement, unanswered. Everything
/// else, and every command on a player this host owns, passes.
#[test]
fn a_cast_holds_back_only_the_commands_that_replace_its_track() {
    let numbered = |act: Option<ListenerAct>| PlayerIpc::Load {
        url: "https://cdn/mediatracks/cast.flac".to_string(),
        format: "flac".to_string(),
        key: String::new(),
        product_id: Some("2".to_string()),
        restart: true,
        want_play: act.is_some(),
        load_id: Some(7),
        act,
    };
    let routed = |act: Option<ListenerAct>| {
        CastGate::Route(CastLoad {
            product_id: "2".to_string(),
            load_id: 7,
            act,
        })
    };

    let rows = [
        (
            every_command()
                .into_iter()
                .chain([sdk_load(Some("2"), true)])
                .collect::<Vec<_>>(),
            LoadOrigin::Local,
            CastGate::Pass,
        ),
        (
            vec![load(), load_dash(), recover()],
            LoadOrigin::Connect,
            CastGate::Refuse,
        ),
        (
            vec![
                PlayerIpc::Stop,
                PlayerIpc::Metadata {
                    payload: json!({ "productId": "2" }),
                },
            ],
            LoadOrigin::Connect,
            CastGate::Drop,
        ),
        (passthroughs(), LoadOrigin::Connect, CastGate::Pass),
        (
            vec![numbered(Some(ListenerAct::Pick))],
            LoadOrigin::Connect,
            routed(Some(ListenerAct::Pick)),
        ),
        (vec![numbered(None)], LoadOrigin::Connect, routed(None)),
        (
            vec![numbered(Some(ListenerAct::Next))],
            LoadOrigin::Connect,
            routed(Some(ListenerAct::Next)),
        ),
        (
            vec![sdk_load(None, true)],
            LoadOrigin::Connect,
            CastGate::Refuse,
        ),
    ];
    for (commands, origin, expected) in rows {
        for command in commands {
            assert_eq!(
                command.cast_gate(origin),
                expected,
                "{command:?} over {origin:?}"
            );
        }
    }
}

/// A load of the SDK delegate's: numbered, and a pick when TIDAL's "play this" preceded it.
fn sdk_load(product_id: Option<&str>, pick: bool) -> PlayerIpc {
    PlayerIpc::Load {
        url: "https://cdn/mediatracks/pick.flac".to_string(),
        format: "flac".to_string(),
        key: String::new(),
        product_id: product_id.map(ToOwned::to_owned),
        restart: true,
        want_play: pick,
        load_id: Some(7),
        act: pick.then_some(ListenerAct::Pick),
    }
}
