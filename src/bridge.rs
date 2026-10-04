use serde::Serialize;

#[derive(Serialize, Debug)]
pub(crate) struct PlayerBridgeEvent {
    pub t: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seq: Option<u32>,
    pub v: serde_json::Value,
    /// Set on a state event while a TIDAL Connect controller owns the player. Such a state
    /// belongs to a track the renderer's queue never chose, and the queue must not act on it.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub cast: bool,
}

impl PlayerBridgeEvent {
    pub fn time(value: f64, seq: u32) -> Self {
        Self {
            t: "time",
            seq: Some(seq),
            v: serde_json::json!(value),
            cast: false,
        }
    }

    pub fn duration(value: f64, seq: u32) -> Self {
        Self {
            t: "duration",
            seq: Some(seq),
            v: serde_json::json!(value),
            cast: false,
        }
    }

    /// `owner` is whoever holds the player as the state is handed over, which is what the
    /// renderer's queue has to know before acting on it.
    pub fn state(value: &'static str, seq: u32, owner: crate::player::LoadOrigin) -> Self {
        Self {
            t: "state",
            seq: Some(seq),
            v: serde_json::json!(value),
            cast: owner == crate::player::LoadOrigin::Connect,
        }
    }

    /// The answer to a load refused while a TIDAL Connect controller owns the player. The renderer
    /// settles the SDK's pending load: a listener's pick as a refusal (the undecodable-profile
    /// branch's), a load TIDAL made on its own or the reload behind a listener's Next/Previous,
    /// quietly.
    ///
    /// `load` names the load answered, and the renderer settles only the load an answer names: a
    /// late answer cannot refuse a newer load. `None` for a self-load, which the SDK does not
    /// wait on.
    pub fn cast_owns_player(load: Option<u64>) -> Self {
        Self {
            t: "mediacastowned",
            seq: None,
            v: load.map_or(
                serde_json::Value::Null,
                |id| serde_json::json!({ "load": id }),
            ),
            cast: false,
        }
    }

    /// The answer to a load of the SDK's that the TIDAL Connect controller's queue plays: a
    /// listener's pick it now plays, or the track it was already playing. The renderer settles
    /// the SDK's pending load as one that ran, with no refusal.
    pub fn cast_routed(load: u64) -> Self {
        Self {
            t: "mediacastrouted",
            seq: None,
            v: serde_json::json!({ "load": load }),
            cast: false,
        }
    }

    pub fn devices(value: serde_json::Value) -> Self {
        Self {
            t: "devices",
            seq: None,
            cast: false,
            v: value,
        }
    }

    pub fn device_error(event_name: &'static str) -> Self {
        Self {
            t: event_name,
            seq: None,
            cast: false,
            v: serde_json::Value::Null,
        }
    }

    pub fn media_format(
        codec: &'static str,
        sample_rate: u32,
        output_sample_rate: u32,
        bit_depth: Option<u32>,
        channels: u16,
        bytes: u64,
    ) -> Self {
        Self {
            t: "mediaformat",
            seq: None,
            cast: false,
            v: serde_json::json!({
                "codec": codec,
                "sampleRate": sample_rate,
                "outputSampleRate": output_sample_rate,
                "bitDepth": bit_depth,
                "channels": channels,
                "bytes": bytes,
            }),
        }
    }

    pub fn version(v: &str) -> Self {
        Self {
            t: "version",
            seq: None,
            cast: false,
            v: serde_json::json!(v),
        }
    }

    pub fn media_error(error: &str, code: &str) -> Self {
        Self {
            t: "mediaerror",
            seq: None,
            cast: false,
            v: serde_json::json!({ "error": error, "errorCode": code }),
        }
    }

    pub fn max_connections() -> Self {
        Self {
            t: "mediamaxconnectionsreached",
            seq: None,
            cast: false,
            v: serde_json::Value::Null,
        }
    }

    /// Not a passthrough event: the renderer answers it with the same five-step refusal
    /// the undecodable-profile branch uses, because a raw `mediaerror` alone loses the
    /// one-second STALLED race TIDAL arms on every autoplay load.
    pub fn network_lost() -> Self {
        Self {
            t: "medianetworklost",
            seq: None,
            cast: false,
            v: serde_json::Value::Null,
        }
    }

    /// The OS mixer moved the level: the whole report, whose count the page's later levels
    /// must carry. `seq` stays the load generation's slot, which this event has none of.
    pub fn volume(report: &crate::player::LevelReport) -> Self {
        Self {
            t: "volume",
            seq: None,
            cast: false,
            v: serde_json::to_value(report).unwrap_or_default(),
        }
    }
}

#[cfg(test)]
#[path = "../tests/unit/bridge.rs"]
mod tests;
