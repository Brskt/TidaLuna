use crate::api;
use crate::{IpcCallback, IpcMessage, ipc_callback_err, ipc_callback_ok, with_state};

/// Try to handle a `tidal.*` IPC channel.
///
/// Returns `true` if the channel was recognised and handled (the
/// callback will be invoked asynchronously). Returns `false` if the
/// channel does not start with `"tidal."`, so the caller can fall
/// through to other handlers.
pub(crate) fn handle_tidal_ipc(msg: IpcMessage, callback: IpcCallback) -> bool {
    if !msg.channel.starts_with("tidal.") {
        return false;
    }

    let IpcMessage {
        channel, args, id, ..
    } = msg;
    let id = id.unwrap_or_default();

    // All tidal.* channels are async — store the callback and spawn
    // a task on the tokio runtime, same pattern as proxy.fetch.
    with_state(|state| {
        state.pending_ipc_callbacks.insert(id.clone(), callback);
        let rt = state.rt_handle.clone();
        rt.spawn(async move {
            dispatch_tidal(channel, args, id).await;
        });
    });

    true
}

/// Route a `tidal.*` channel to the corresponding API function and
/// send the JSON result back through the IPC callback.
async fn dispatch_tidal(channel: String, args: Vec<serde_json::Value>, id: String) {
    let result = match channel.as_str() {
        // ---------------------------------------------------------------
        // Credentials — must be called before any other tidal.* channel
        // ---------------------------------------------------------------
        "tidal.set_credentials" => {
            handle_set_credentials(&args);
            reply(&id, Ok("true".to_string()));
            return;
        }

        // ---------------------------------------------------------------
        // Track endpoints
        // ---------------------------------------------------------------
        "tidal.track" => {
            let track_id = arg_str(&args, 0);
            serialize_option(api::track::track(&track_id).await)
        }
        "tidal.playback_info" => {
            let track_id = arg_str(&args, 0);
            let quality = arg_audio_quality(&args, 1);
            serialize_option(api::track::playback_info(&track_id, quality).await)
        }
        "tidal.playback_info_raw" => {
            let track_id = arg_str(&args, 0);
            let quality = arg_audio_quality(&args, 1);
            serialize_option(api::track::playback_info_raw(&track_id, quality).await)
        }
        "tidal.lyrics" => {
            let track_id = arg_str(&args, 0);
            serialize_option(api::track::lyrics(&track_id).await)
        }
        "tidal.isrc" => {
            let isrc = arg_str(&args, 0);
            serialize_result(api::track::isrc(&isrc).await)
        }

        // ---------------------------------------------------------------
        // Album endpoints
        // ---------------------------------------------------------------
        "tidal.album" => {
            let album_id = arg_str(&args, 0);
            serialize_option(api::album::album(&album_id).await)
        }
        "tidal.album_items" => {
            let album_id = arg_str(&args, 0);
            serialize_option(api::album::album_items(&album_id).await)
        }

        // ---------------------------------------------------------------
        // Artist endpoint
        // ---------------------------------------------------------------
        "tidal.artist" => {
            let artist_id = arg_str(&args, 0);
            serialize_option(api::artist::artist(&artist_id).await)
        }

        // ---------------------------------------------------------------
        // Playlist endpoints
        // ---------------------------------------------------------------
        "tidal.playlist" => {
            let uuid = arg_str(&args, 0);
            serialize_option(api::playlist::playlist(&uuid).await)
        }
        "tidal.playlist_items" => {
            let uuid = arg_str(&args, 0);
            serialize_option(api::playlist::playlist_items(&uuid).await)
        }

        // ---------------------------------------------------------------
        // Content helpers
        // ---------------------------------------------------------------
        "tidal.cover_url" => {
            let uuid = arg_str(&args, 0);
            let res = args.get(1).and_then(|v| v.as_str()).unwrap_or("1280");
            let cover_type = args.get(2).and_then(|v| v.as_str()).unwrap_or("image");

            let opts = api::content::CoverOpts {
                res: match res {
                    "80" => api::content::CoverRes::R80,
                    "160" => api::content::CoverRes::R160,
                    "320" => api::content::CoverRes::R320,
                    "640" => api::content::CoverRes::R640,
                    _ => api::content::CoverRes::R1280,
                },
                cover_type: match cover_type {
                    "video" => api::content::CoverType::Video,
                    _ => api::content::CoverType::Image,
                },
            };

            let url = api::content::format_cover_url(&uuid, Some(&opts));
            Ok(serde_json::to_string(&url).unwrap_or_else(|_| "null".to_string()))
        }

        // ---------------------------------------------------------------
        // Tags
        // ---------------------------------------------------------------
        "tidal.tags" => {
            let track_id = arg_str(&args, 0);
            serialize_result(api::tags::make_tags(&track_id).await)
        }

        // ---------------------------------------------------------------
        // Unknown tidal.* channel
        // ---------------------------------------------------------------
        _ => Err(anyhow::anyhow!("Unknown tidal channel: {channel}")),
    };

    reply(&id, result);
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Extract a string argument at the given index, stripping JSON quotes.
fn arg_str(args: &[serde_json::Value], index: usize) -> String {
    args.get(index)
        .map(|v| match v.as_str() {
            Some(s) => s.to_string(),
            None => {
                // Numeric IDs come as JSON numbers — convert to string
                let s = v.to_string();
                s.trim_matches('"').to_string()
            }
        })
        .unwrap_or_default()
}

/// Parse the AudioQuality argument, defaulting to Lossless.
fn arg_audio_quality(
    args: &[serde_json::Value],
    index: usize,
) -> api::types::quality::AudioQuality {
    args.get(index)
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or(api::types::quality::AudioQuality::Lossless)
}

/// Set credentials from IPC args: [token, clientId, countryCode, locale]
fn handle_set_credentials(args: &[serde_json::Value]) {
    let token = arg_str(args, 0);
    let client_id = arg_str(args, 1);
    let country_code = arg_str(args, 2);
    let locale = arg_str(args, 3);
    api::client::set_credentials(api::client::TidalCredentials {
        token,
        client_id,
        country_code,
        locale,
    });
}

/// Serialize an `anyhow::Result<Option<T>>` to JSON.
/// `Ok(Some(val))` → JSON string, `Ok(None)` → `"null"`.
fn serialize_option<T: serde::Serialize>(
    result: anyhow::Result<Option<T>>,
) -> anyhow::Result<String> {
    match result {
        Ok(Some(val)) => Ok(serde_json::to_string(&val)?),
        Ok(None) => Ok("null".to_string()),
        Err(e) => Err(e),
    }
}

/// Serialize an `anyhow::Result<T>` to JSON.
fn serialize_result<T: serde::Serialize>(result: anyhow::Result<T>) -> anyhow::Result<String> {
    match result {
        Ok(val) => Ok(serde_json::to_string(&val)?),
        Err(e) => Err(e),
    }
}

/// Send a response (success or error) through the IPC callback.
fn reply(id: &str, result: anyhow::Result<String>) {
    let id = id.to_string();
    with_state(|state| {
        if let Some(cb) = state.pending_ipc_callbacks.remove(&id) {
            match &result {
                Ok(json) => ipc_callback_ok(&cb, json),
                Err(e) => ipc_callback_err(&cb, &format!("{e:#}")),
            }
        }
    });
}
