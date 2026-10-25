//! Every renderer path that cut a cast ended on one of these channels: this page's auto-advance
//! and self-load, TIDAL's queue sagas, a page reload's cold restore, the SDK's reset before each
//! load. Pinned on the command type: the handler that applies the rule needs the whole `AppState`.

use super::{CastGate, CastLoad, ListenerAct, PlayerIpc};
use crate::player::{LoadOrigin, OutputMode, SegmentUrls};
use serde_json::json;

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
