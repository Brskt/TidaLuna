use serde::Serialize;

#[derive(Serialize, Debug)]
pub(crate) struct PlayerBridgeEvent {
    pub t: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seq: Option<u32>,
    pub v: serde_json::Value,
}

impl PlayerBridgeEvent {
    pub fn time(value: f64, seq: u32) -> Self {
        Self {
            t: "time",
            seq: Some(seq),
            v: serde_json::json!(value),
        }
    }

    pub fn duration(value: f64, seq: u32) -> Self {
        Self {
            t: "duration",
            seq: Some(seq),
            v: serde_json::json!(value),
        }
    }

    pub fn state(value: &'static str, seq: u32) -> Self {
        Self {
            t: "state",
            seq: Some(seq),
            v: serde_json::json!(value),
        }
    }

    pub fn devices(value: serde_json::Value) -> Self {
        Self {
            t: "devices",
            seq: None,
            v: value,
        }
    }
}

pub(crate) fn flush_player_bridge(
    webview: &wry::WebView,
    pending_time_update: &mut Option<(f64, u32)>,
    pending_player_events: &mut Vec<PlayerBridgeEvent>,
    pending_misc_js: &mut Vec<String>,
) {
    if let Some((time, seq)) = pending_time_update.take() {
        pending_player_events.push(PlayerBridgeEvent::time(time, seq));
    }

    if !pending_player_events.is_empty() {
        match serde_json::to_string(&*pending_player_events) {
            Ok(events_json) => {
                let js = format!(
                    "if (window.__TIDAL_RS_PLAYER_PUSH__) {{ window.__TIDAL_RS_PLAYER_PUSH__({}); }}",
                    events_json
                );
                let _ = webview.evaluate_script(&js);
            }
            Err(e) => eprintln!("[BRIDGE] Failed to serialize player events: {e}"),
        }
        pending_player_events.clear();
    }

    if !pending_misc_js.is_empty() {
        let js_batch = pending_misc_js.join(";");
        let _ = webview.evaluate_script(&js_batch);
        pending_misc_js.clear();
    }
}
