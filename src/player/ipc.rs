use serde_json::Value;

#[derive(Debug, PartialEq)]
pub(crate) enum PlayerIpc {
    Load {
        url: String,
        format: String,
        key: String,
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
        exclusive: bool,
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
        "player.dbg" => {
            if let Some(msg) = args.first().and_then(|v| v.as_str()) {
                crate::vprintln!("{msg}");
            }
            Err(PlayerIpcParseError::UnknownChannel(
                "player.dbg".to_string(),
            ))
        }
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
                let exclusive = args
                    .get(1)
                    .and_then(|v| v.as_str())
                    .is_some_and(|mode| mode == "exclusive");
                PlayerIpc::DevicesSet {
                    id: id.to_string(),
                    exclusive,
                }
            })
            .ok_or(PlayerIpcParseError::InvalidArgs("player.devices.set")),
        _ => Err(PlayerIpcParseError::UnknownChannel(channel.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::{PlayerIpc, parse_player_ipc};
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
                exclusive: true,
            }
        );
    }

    #[test]
    fn errors_on_invalid_required_args() {
        assert!(parse_player_ipc("player.load", &[json!("https://a")], None).is_err());
        assert!(parse_player_ipc("player.metadata", &[], None).is_err());
    }
}
