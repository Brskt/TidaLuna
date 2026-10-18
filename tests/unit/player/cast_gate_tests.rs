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

/// No receiver, a phone attached but not casting, or a session that ended: `Local` stays
/// current, every command passes (a numbered pick included), and local gapless and crossfade
/// stay untouched by the gate.
#[test]
fn this_host_owning_the_player_passes_every_command() {
    for command in every_command()
        .into_iter()
        .chain([sdk_load(Some("2"), true)])
    {
        assert_eq!(
            command.cast_gate(LoadOrigin::Local),
            CastGate::Pass,
            "{command:?} was held back from a player this host owns"
        );
    }
}

#[test]
fn a_cast_refuses_every_load_the_renderer_asks_for() {
    for command in [load(), load_dash(), recover()] {
        assert_eq!(
            command.cast_gate(LoadOrigin::Connect),
            CastGate::Refuse,
            "{command:?} would replace the controller's track"
        );
    }
}

/// TIDAL's SDK sends a stop ahead of every load and from its own error handling, the refusal's
/// included.
#[test]
fn a_cast_drops_the_stop_and_the_announcement_without_answering() {
    let stop = PlayerIpc::Stop;
    assert_eq!(stop.cast_gate(LoadOrigin::Connect), CastGate::Drop);
    let metadata = PlayerIpc::Metadata {
        payload: json!({ "productId": "2" }),
    };
    assert_eq!(metadata.cast_gate(LoadOrigin::Connect), CastGate::Drop);
}

#[test]
fn a_cast_keeps_every_command_that_leaves_its_track_in_place() {
    for command in passthroughs() {
        assert_eq!(
            command.cast_gate(LoadOrigin::Connect),
            CastGate::Pass,
            "{command:?} does not replace the controller's track"
        );
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

/// A pick, an automatic load and a listener's Next all reach the receiver with the act they
/// answer. The automatic one is routed, not refused: only the receiver knows whether it names the
/// track the cast already plays. A Next reloads the page's entry, and the receiver steps the cast
/// for it.
#[test]
fn a_numbered_load_during_a_cast_goes_to_the_receiver_with_its_act() {
    for act in [Some(ListenerAct::Pick), None, Some(ListenerAct::Next)] {
        let load = PlayerIpc::Load {
            url: "https://cdn/mediatracks/cast.flac".to_string(),
            format: "flac".to_string(),
            key: String::new(),
            product_id: Some("2".to_string()),
            restart: true,
            want_play: act.is_some(),
            load_id: Some(7),
            act,
        };
        assert_eq!(
            load.cast_gate(LoadOrigin::Connect),
            CastGate::Route(CastLoad {
                product_id: "2".to_string(),
                load_id: 7,
                act,
            }),
            "{act:?}"
        );
    }
}

#[test]
fn a_load_naming_no_track_is_refused() {
    assert_eq!(
        sdk_load(None, true).cast_gate(LoadOrigin::Connect),
        CastGate::Refuse
    );
}

#[test]
fn only_the_delegates_load_carries_its_number() {
    assert_eq!(sdk_load(Some("2"), true).load_id(), Some(7));
    assert_eq!(load().load_id(), None);
    assert_eq!(load_dash().load_id(), None);
    assert_eq!(recover().load_id(), None);
}
