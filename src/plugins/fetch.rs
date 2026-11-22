/// Async handler for `plugin.fetch`: sends the request `plugins::outbound` admitted, with the
/// OAuth token injected server-side for TIDAL API hosts (the plugin never sees it), and returns a
/// Response-like JSON.
use crate::plugins::outbound::{OutboundRequest, PluginFetchError};

/// Execute a plugin fetch request.
///
/// `token` - OAuth token to inject for Tidal API calls (empty = no injection).
/// Returns a JSON string with `{ok, status, statusText, url, headers, body}`.
pub(crate) async fn plugin_fetch(
    plugin_id: &str,
    request: OutboundRequest,
    token: &str,
) -> Result<String, PluginFetchError> {
    let url = request.url().as_str().to_string();
    let method = request.method();
    let is_tidal = is_tidal_api(&url);
    // The plugin never sees the token, and its own Authorization header wins.
    let inject = !token.is_empty() && is_tidal && !request.has_header("authorization");
    let mut req = crate::state::PLUGIN_CLIENT.send(request);
    if inject {
        req = req.header("Authorization", format!("Bearer {token}"));
    }

    crate::vprintln!(
        "[PLUGIN:FETCH] {} {} {} ({})",
        plugin_id,
        method.as_str(),
        crate::util::redact_url_query(&url),
        if is_tidal {
            "token injected"
        } else {
            "no token"
        }
    );

    let resp = req.send().await.map_err(PluginFetchError::of_send)?;

    let status = resp.status().as_u16();
    let status_text = resp.status().canonical_reason().unwrap_or("").to_string();
    let ok = resp.status().is_success();
    let final_url = resp.url().to_string();

    // Collect response headers
    let mut resp_headers = serde_json::Map::new();
    for (k, v) in resp.headers() {
        if let Ok(v) = v.to_str() {
            resp_headers.insert(
                k.as_str().to_string(),
                serde_json::Value::String(v.to_string()),
            );
        }
    }

    let body_text = resp
        .text()
        .await
        .map_err(|e| PluginFetchError::Body(crate::util::network_error_text(e)))?;

    let result = serde_json::json!({
        "status": status,
        "statusText": status_text,
        "ok": ok,
        "url": final_url,
        "headers": resp_headers,
        "body": body_text,
    });

    Ok(result.to_string())
}

/// Check if a URL points to Tidal's API (should receive the OAuth token).
/// HTTPS is part of the answer, not a separate concern: a true verdict attaches the real
/// access token, and this path runs from IPC straight into reqwest, never through Blink, so
/// no mixed-content check would upgrade or refuse a plaintext scheme on the way out.
pub(crate) fn is_tidal_api(url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    parsed.scheme() == "https" && crate::ui::nav::is_tidal_api_host(parsed.host_str().unwrap_or(""))
}

#[cfg(test)]
#[path = "../../tests/unit/plugins/fetch.rs"]
mod tests;
