use serde_json::Value;

use super::OutputMode;

#[derive(Debug, PartialEq)]
pub(crate) enum PlayerIpc {
    Load {
        url: String,
        format: String,
        key: String,
        /// The frontend's own id for this track. Empty is read as absent: a blank id would
        /// compare equal to another blank one and lend a length across tracks.
        product_id: Option<String>,
        restart: bool,
        want_play: bool,
        /// The SDK delegate's own number for this load, which an answer to it names. Absent
        /// from the self-loads, which no SDK load waits on.
        load_id: Option<u64>,
        /// The listener's act this load answers, `None` for an automatic load (a queue advance, a
        /// restore, the autoplay mix at a queue's end).
        act: Option<ListenerAct>,
    },
    Recover {
        url: String,
        format: String,
        key: String,
        target_time: Option<f64>,
    },
    LoadDash {
        init_url: String,
        segment_urls: Vec<String>,
        format: String,
        product_id: Option<String>,
    },
    Preload {
        url: String,
        format: String,
        key: String,
        /// The track this preload is for, read off the play queue by the renderer. The
        /// gapless advance is driven in Rust, so this is the only chance to name it.
        product_id: Option<String>,
    },
    PreloadCancel,
    Metadata {
        payload: Value,
    },
    Play,
    Pause,
    Stop,
    Seek {
        time: f64,
    },
    Volume {
        volume: f64,
        /// The newest level report the page had taken in when it decided this one; absent
        /// from a sender that keeps no count (a plugin).
        seen: Option<u64>,
    },
    DevicesGet {
        request_id: Option<String>,
    },
    DevicesSet {
        id: String,
        mode: OutputMode,
    },
}

/// What a renderer command may do while a TIDAL Connect controller owns the player.
///
/// The renderer's queue, TIDAL's sagas and the SDK all go on running during a cast, and none of
/// them knows about it: a load one of them issues replaces the cast's track, and a stop halts it.
/// The rule lives here, on the one road all of them share, rather than at each sender.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CastGate {
    /// This host owns the player, or the command does not take it over: it runs.
    Pass,
    /// A load of the SDK's while a cast owns the player. Not run here: the receiver, the one
    /// owner of the controller's queue, decides what it becomes and answers it.
    Route(CastLoad),
    /// Any other load while a cast owns the player. Not run, and answered: the SDK waits on a
    /// load it issued with no timeout.
    Refuse,
    /// Neither run nor answered: a stop is fire-and-forget, and a metadata frame would name a
    /// track other than the one the controller is playing, which the receiver announced itself.
    Drop,
}

/// A listener's act on the page's play queue that an SDK load answers: TIDAL turns each into a
/// load of the entry it lands on, and during a cast the receiver plays it on the cast instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ListenerAct {
    /// TIDAL's "play this" on a track.
    Pick,
    Next,
    /// The player's Previous, when it moves the queue rather than restarting the track.
    Previous,
}

impl ListenerAct {
    /// The renderer's name for the act (`ListenerAct` in `frontend/src/controllers/player.ts`).
    /// Anything else is no act, which is what a load from any sender but the SDK delegate carries.
    fn from_wire(value: &Value) -> Option<Self> {
        match value.as_str()? {
            "pick" => Some(Self::Pick),
            "next" => Some(Self::Next),
            "previous" => Some(Self::Previous),
            _ => None,
        }
    }
}

/// A load of the SDK's handed to the TIDAL Connect receiver: the track, the load to answer,
/// and the listener's act it answers, if any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CastLoad {
    pub product_id: String,
    pub load_id: u64,
    pub act: Option<ListenerAct>,
}

impl PlayerIpc {
    /// The `load_id` of a `player.load`, `None` for every other command.
    pub(crate) fn load_id(&self) -> Option<u64> {
        match self {
            PlayerIpc::Load { load_id, .. } => *load_id,
            _ => None,
        }
    }

    /// Decided by who owns the player, never by who is attached: a phone connected but not
    /// casting leaves this host's own load current, and every command passes.
    pub(crate) fn cast_gate(&self, owner: super::LoadOrigin) -> CastGate {
        if owner == super::LoadOrigin::Local {
            return CastGate::Pass;
        }
        match self {
            // The receiver plays a listener's act on the cast and settles a load of the track the
            // cast already plays; it refuses the rest. A load naming no track gives it nothing to
            // look up, and one with no number is a self-load, which no SDK load waits on.
            PlayerIpc::Load {
                product_id: Some(product_id),
                load_id: Some(load_id),
                act,
                ..
            } => CastGate::Route(CastLoad {
                product_id: product_id.clone(),
                load_id: *load_id,
                act: *act,
            }),
            PlayerIpc::Load { .. } | PlayerIpc::LoadDash { .. } | PlayerIpc::Recover { .. } => {
                CastGate::Refuse
            }
            PlayerIpc::Stop | PlayerIpc::Metadata { .. } => CastGate::Drop,
            // Transport and level commands act on whatever is playing, the cast included, as
            // the controller's own would. A staged preload is refused where it would take
            // over, at promotion (`next_is_current`), and the device commands keep the track.
            PlayerIpc::Play
            | PlayerIpc::Pause
            | PlayerIpc::Seek { .. }
            | PlayerIpc::Volume { .. }
            | PlayerIpc::Preload { .. }
            | PlayerIpc::PreloadCancel
            | PlayerIpc::DevicesGet { .. }
            | PlayerIpc::DevicesSet { .. } => CastGate::Pass,
        }
    }
}

#[derive(Debug, PartialEq)]
pub(crate) enum PlayerIpcParseError {
    InvalidArgs(&'static str),
    UnknownChannel(String),
}

/// A track id the renderer may or may not have, read exactly as the metadata frame reads its
/// own: the renderer forwards TIDAL's `mediaProduct.productId` untouched, and that is a number
/// as often as a string. Reading only strings here tagged those loads with nothing while the
/// metadata frame named the same track, so `same_track` could never match.
fn optional_id(arg: Option<&Value>) -> Option<String> {
    arg.and_then(crate::util::metadata::value_track_id)
}

fn parse_player_recover_args(args: &[Value]) -> Option<(String, String, String, Option<f64>)> {
    fn parse_num(v: &Value) -> Option<f64> {
        v.as_f64()
            .or_else(|| v.as_str().and_then(|s| s.trim().parse::<f64>().ok()))
    }

    fn parse_millis(v: &Value) -> Option<f64> {
        parse_num(v).map(|ms| ms / 1000.0)
    }

    let payload = args.iter().find(|v| v.is_object());

    let url = payload
        .and_then(|p| p.get("url"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| {
            args.first()
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(ToOwned::to_owned)
        })?;

    let mut format = payload
        .and_then(|p| {
            p.get("streamFormat")
                .and_then(|v| v.as_str())
                .or_else(|| p.get("format").and_then(|v| v.as_str()))
        })
        .unwrap_or("flac")
        .to_string();

    let mut key = payload
        .and_then(|p| {
            p.get("encryptionKey")
                .and_then(|v| v.as_str())
                .or_else(|| p.get("key").and_then(|v| v.as_str()))
        })
        .unwrap_or("")
        .to_string();

    if payload.is_none() {
        if let (Some(arg1), Some(arg2)) = (
            args.get(1).and_then(|v| v.as_str()),
            args.get(2).and_then(|v| v.as_str()),
        ) {
            format = arg1.to_string();
            key = arg2.to_string();
        } else if let Some(arg1) = args.get(1).and_then(|v| v.as_str()) {
            key = arg1.to_string();
        }
    }

    if format.is_empty() {
        format = "flac".to_string();
    }

    let payload_time = payload.and_then(|p| {
        p.get("currentTime")
            .and_then(parse_num)
            .or_else(|| p.get("time").and_then(parse_num))
            .or_else(|| p.get("position").and_then(parse_num))
            .or_else(|| p.get("seek").and_then(parse_num))
            .or_else(|| p.get("startPosition").and_then(parse_num))
            .or_else(|| p.get("resumeTime").and_then(parse_num))
            .or_else(|| p.get("positionMs").and_then(parse_millis))
            .or_else(|| p.get("timeMs").and_then(parse_millis))
    });

    let numeric_arg = args.iter().find_map(parse_num);
    let target_time = payload_time
        .or(numeric_arg)
        .filter(|t| t.is_finite() && *t > 0.0);

    Some((url, format, key, target_time))
}

pub(crate) fn parse_player_ipc(
    channel: &str,
    args: &[Value],
    request_id: Option<&str>,
) -> Result<PlayerIpc, PlayerIpcParseError> {
    match channel {
        "player.load" => match (
            args.first().and_then(|v| v.as_str()),
            args.get(1).and_then(|v| v.as_str()),
            args.get(2).and_then(|v| v.as_str()),
        ) {
            (Some(url), Some(format), Some(key)) => Ok(PlayerIpc::Load {
                url: url.to_string(),
                format: format.to_string(),
                key: key.to_string(),
                // 4th arg (optional): the renderer sets it when TIDAL minted a new
                // mediaProduct.referenceId for this play instance: a same-track
                // re-load must restart at 0 rather than resume in place.
                restart: args.get(3).and_then(|v| v.as_bool()).unwrap_or(false),
                // 5th arg (optional): a track/list SELECT wants the loaded track to
                // auto-play; folded here instead of a separate player.play that would
                // resume the old committed track. Applies only to a different-track load.
                want_play: args.get(4).and_then(|v| v.as_bool()).unwrap_or(false),
                // 6th arg (optional): the track this load is for. A length measured here
                // can be published under that track and no other.
                product_id: optional_id(args.get(5)),
                // 7th and 8th args (optional): sent by the SDK's load delegate alone, the one
                // sender whose loads an answer has to name.
                load_id: args.get(6).and_then(|v| v.as_u64()),
                act: args.get(7).and_then(ListenerAct::from_wire),
            }),
            _ => Err(PlayerIpcParseError::InvalidArgs("player.load")),
        },
        "player.load_dash" => {
            let init_url = args.first().and_then(|v| v.as_str()).unwrap_or_default();
            let segment_urls: Vec<String> = args
                .get(1)
                .and_then(|v| v.as_str())
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or_default();
            let format = args.get(2).and_then(|v| v.as_str()).unwrap_or("aac");
            if init_url.is_empty() || segment_urls.is_empty() {
                Err(PlayerIpcParseError::InvalidArgs("player.load_dash"))
            } else {
                Ok(PlayerIpc::LoadDash {
                    init_url: init_url.to_string(),
                    segment_urls,
                    format: format.to_string(),
                    product_id: optional_id(args.get(3)),
                })
            }
        }
        "player.recover" => parse_player_recover_args(args)
            .map(|(url, format, key, target_time)| PlayerIpc::Recover {
                url,
                format,
                key,
                target_time,
            })
            .ok_or(PlayerIpcParseError::InvalidArgs("player.recover")),
        "player.preload" => match (
            args.first().and_then(|v| v.as_str()),
            args.get(1).and_then(|v| v.as_str()),
            args.get(2).and_then(|v| v.as_str()),
        ) {
            (Some(url), Some(format), Some(key)) => Ok(PlayerIpc::Preload {
                url: url.to_string(),
                format: format.to_string(),
                key: key.to_string(),
                // 4th arg (optional): older renderers send three. An absent id lands the
                // track untagged, which is what this channel did for every preload before.
                product_id: optional_id(args.get(3)),
            }),
            _ => Err(PlayerIpcParseError::InvalidArgs("player.preload")),
        },
        "player.preload.cancel" => Ok(PlayerIpc::PreloadCancel),
        "player.metadata" => args
            .first()
            .cloned()
            .map(|payload| PlayerIpc::Metadata { payload })
            .ok_or(PlayerIpcParseError::InvalidArgs("player.metadata")),
        "player.play" => Ok(PlayerIpc::Play),
        "player.pause" => Ok(PlayerIpc::Pause),
        "player.stop" => Ok(PlayerIpc::Stop),
        "player.seek" => args
            .first()
            .and_then(|v| v.as_f64())
            .map(|time| PlayerIpc::Seek { time })
            .ok_or(PlayerIpcParseError::InvalidArgs("player.seek")),
        "player.volume" => args
            .first()
            .and_then(|v| v.as_f64())
            .map(|volume| PlayerIpc::Volume {
                volume,
                seen: args.get(1).and_then(|v| v.as_u64()),
            })
            .ok_or(PlayerIpcParseError::InvalidArgs("player.volume")),
        "player.devices.get" => Ok(PlayerIpc::DevicesGet {
            request_id: request_id.map(ToOwned::to_owned),
        }),
        "player.devices.set" => args
            .first()
            .and_then(|v| v.as_str())
            .map(|id| {
                let mode = match args.get(1).and_then(|v| v.as_str()) {
                    Some("exclusive") => OutputMode::Exclusive,
                    Some("asio") => OutputMode::Asio,
                    _ => OutputMode::Shared,
                };
                PlayerIpc::DevicesSet {
                    id: id.to_string(),
                    mode,
                }
            })
            .ok_or(PlayerIpcParseError::InvalidArgs("player.devices.set")),
        _ => Err(PlayerIpcParseError::UnknownChannel(channel.to_string())),
    }
}

#[cfg(test)]
#[path = "../../tests/unit/player/ipc.rs"]
mod tests;

#[cfg(test)]
#[path = "../../tests/unit/player/cast_gate_tests.rs"]
mod cast_gate_tests;
