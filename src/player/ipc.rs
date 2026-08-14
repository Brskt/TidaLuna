use serde_json::Value;

use super::OutputMode;

#[derive(Debug, PartialEq)]
pub(crate) enum PlayerIpc {
    Load {
        url: String,
        format: String,
        key: String,
        restart: bool,
        want_play: bool,
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
    },
    Preload {
        url: String,
        format: String,
        key: String,
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
    },
    DevicesGet {
        request_id: Option<String>,
    },
    DevicesSet {
        id: String,
        mode: OutputMode,
    },
}

#[derive(Debug, PartialEq)]
pub(crate) enum PlayerIpcParseError {
    InvalidArgs(&'static str),
    UnknownChannel(String),
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
            .map(|volume| PlayerIpc::Volume { volume })
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
