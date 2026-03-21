/// Async fetch handler for plugin.fetch IPC channel.
///
/// Plugins call `fetch(url, opts)` which the security wrapper routes
/// through cefQuery → Rust. This handler:
///   1. Parses the request (method, headers, body)
///   2. Injects the OAuth token for Tidal API requests
///   3. Executes the fetch via reqwest (async)
///   4. Returns a Response-like JSON
///
/// The token is never exposed to the plugin — it's injected server-side.
use serde::Deserialize;

#[derive(Deserialize)]
pub(crate) struct FetchOpts {
    #[serde(default = "default_method")]
    pub method: String,
    #[serde(default)]
    pub headers: serde_json::Map<String, serde_json::Value>,
    #[serde(default)]
    pub body: Option<String>,
}

fn default_method() -> String {
    "GET".to_string()
}

/// Execute a plugin fetch request.
///
/// `token` — OAuth token to inject for Tidal API calls (empty = no injection).
/// Returns a JSON string with `{ok, status, statusText, url, headers, body}`.
pub(crate) async fn plugin_fetch(
    plugin_id: &str,
    url: &str,
    opts: &FetchOpts,
    token: &str,
) -> Result<String, String> {
    let client = &*crate::state::HTTP_CLIENT;

    let mut req = match opts.method.to_uppercase().as_str() {
        "POST" => client.post(url),
        "PUT" => client.put(url),
        "DELETE" => client.delete(url),
        "PATCH" => client.patch(url),
        "HEAD" => client.head(url),
        _ => client.get(url),
    };

    // Apply headers from the plugin request
    for (key, value) in &opts.headers {
        if let Some(val) = value.as_str() {
            req = req.header(key.as_str(), val);
        }
    }

    // Inject OAuth token for Tidal API requests (plugin never sees the token)
    if !token.is_empty() && is_tidal_api(url) {
        // Only inject if plugin didn't already provide an Authorization header
        if !opts
            .headers
            .keys()
            .any(|k| k.eq_ignore_ascii_case("authorization"))
        {
            req = req.header("Authorization", format!("Bearer {token}"));
        }
    }

    // Apply body
    if let Some(ref body) = opts.body {
        req = req.body(body.clone());
    }

    crate::vprintln!(
        "[PLUGIN:FETCH] {} {} {} ({})",
        plugin_id,
        opts.method,
        url,
        if is_tidal_api(url) {
            "token injected"
        } else {
            "no token"
        }
    );

    let resp = req
        .send()
        .await
        .map_err(|e| format!("plugin.fetch failed: {e}"))?;

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

    let body_text = resp.text().await.unwrap_or_default();

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
fn is_tidal_api(url: &str) -> bool {
    let host = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .and_then(|s| s.split('/').next())
        .unwrap_or("");

    host == "api.tidal.com" || host == "api.tidalhifi.com" || host == "listen.tidal.com"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_tidal_api() {
        assert!(is_tidal_api("https://api.tidal.com/v1/tracks/12345"));
        assert!(is_tidal_api("https://api.tidalhifi.com/v1/tracks/12345"));
        assert!(is_tidal_api("https://listen.tidal.com/v1/tracks"));
        assert!(!is_tidal_api("https://example.com/api"));
        assert!(!is_tidal_api("https://evil.com/api.tidal.com"));
    }

    #[test]
    fn test_parse_fetch_opts() {
        let json = r#"{"method":"POST","headers":{"Content-Type":"application/json"},"body":"{\"key\":\"val\"}"}"#;
        let opts: FetchOpts = serde_json::from_str(json).unwrap();
        assert_eq!(opts.method, "POST");
        assert_eq!(
            opts.headers.get("Content-Type").unwrap().as_str().unwrap(),
            "application/json"
        );
        assert!(opts.body.is_some());
    }

    #[test]
    fn test_parse_fetch_opts_defaults() {
        let json = r#"{}"#;
        let opts: FetchOpts = serde_json::from_str(json).unwrap();
        assert_eq!(opts.method, "GET");
        assert!(opts.headers.is_empty());
        assert!(opts.body.is_none());
    }
}
