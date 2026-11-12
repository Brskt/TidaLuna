use super::{ipc_callback_err, ipc_callback_ok, take_ipc_callback};
use crate::app_state::{IpcCallback, IpcMessage, with_state};
use crate::plugins::store::StoredPlugin;
use std::time::Duration;

/// Bounds a version's fetches together: the shared client has no read timeout, and a source
/// stalling its body would hold for good whatever waits on it (an update turn, the poll).
const FETCH_TIMEOUT: Duration = Duration::from_secs(30);

pub(super) fn handle_plugin_fetch_package(msg: &IpcMessage, callback: IpcCallback) {
    let url = msg.arg(0).to_string();
    crate::state::rt_handle().spawn(async move {
        let client = &*crate::state::HTTP_CLIENT;
        match within_fetch_timeout(fetch_plugin_package(client, &url)).await {
            Ok(Some(manifest)) => ipc_callback_ok(&callback, &manifest),
            Ok(None) => ipc_callback_err(&callback, 500, "plugin manifest not found"),
            Err(e) => ipc_callback_err(&callback, 500, &e),
        }
    });
}

/// Check for code changes without modifying the DB. Returns `{ hash: "..." }` or error.
/// Used by live reload polling.
pub(super) fn handle_plugin_check_hash(msg: &IpcMessage, query_id: i64, callback: IpcCallback) {
    let url = msg.arg(0).to_string();
    with_state(|state| {
        state.pending_ipc_callbacks.insert(query_id, callback);
    });
    crate::state::rt_handle().spawn(async move {
        let result = served_code_hash(&url).await;

        // After the fetch, not before: a cancellation mid-flight must find the entry still in
        // the map for `on_query_canceled` to drop, or this task resolves a retired callback.
        let Some(cb) = take_ipc_callback(query_id) else {
            return;
        };

        match result {
            Ok(hash) => {
                let json = format!(r#"{{"hash":"{hash}"}}"#);
                ipc_callback_ok(&cb, &json);
            }
            Err(e) => ipc_callback_err(&cb, 500, &e),
        }
    });
}

/// The fnv of the code the source serves, fetched as `plugin.update` fetches it.
async fn served_code_hash(url: &str) -> Result<String, String> {
    let key = url.to_string();
    let stored = tokio::task::spawn_blocking(move || {
        crate::state::db().call_plugins(move |pc| crate::plugins::store::stored(pc, &key))
    })
    .await
    .map_err(|e| format!("spawn_blocking failed: {e}"))?
    .map_err(|e| format!("DB error: {e}"))?;
    fetch_version(url, stored.as_ref())
        .await
        .map(|fetched| fetched.hash)
}

/// [`fetch_plugin_data`] on the shared client, within [`FETCH_TIMEOUT`].
pub(super) async fn fetch_version(
    url: &str,
    stored: Option<&StoredPlugin>,
) -> Result<PluginData, String> {
    let client = &*crate::state::HTTP_CLIENT;
    within_fetch_timeout(fetch_plugin_data(client, url, stored)).await
}

async fn within_fetch_timeout<T>(
    fetch: impl Future<Output = anyhow::Result<T>>,
) -> Result<T, String> {
    tokio::time::timeout(FETCH_TIMEOUT, fetch)
        .await
        .map_err(|_| {
            format!(
                "The source did not answer within {} s",
                FETCH_TIMEOUT.as_secs()
            )
        })?
        .map_err(|e| format!("{e:#}"))
}

/// Whether the stored code is still what the source serves: the fetched manifest carries the
/// stored one's `hash`, and the stored code hashes to it. A pair stored mid-publish (new
/// manifest, old code) fails the second test, so its code is downloaded again.
pub(super) fn serves_stored_code(
    stored_manifest: &str,
    stored_code: &str,
    fetched_manifest: &str,
) -> bool {
    match (
        manifest_hash(stored_manifest),
        manifest_hash(fetched_manifest),
    ) {
        (Some(stored), Some(fetched)) => stored == fetched && content_hash(stored_code) == stored,
        _ => false,
    }
}

fn manifest_hash(manifest: &str) -> Option<String> {
    let manifest: serde_json::Value = serde_json::from_str(manifest).ok()?;
    manifest.get("hash")?.as_str().map(str::to_string)
}

/// The `hash` a Luna plugin build writes into its manifest: esbuild's output hash, xxHash64 of
/// the `.mjs` bytes, little-endian, in base64 without padding.
fn content_hash(code: &str) -> String {
    use base64::Engine as _;
    let digest = twox_hash::XxHash64::oneshot(0, code.as_bytes());
    base64::engine::general_purpose::STANDARD_NO_PAD.encode(digest.to_le_bytes())
}

fn fnv_hash_str(s: &str) -> String {
    use std::hash::{Hash, Hasher as _};
    let mut hasher = fnv::FnvHasher::default();
    s.hash(&mut hasher);
    format!("{:x}", hasher.finish())
}

fn sanitize_plugin_url(url: &str) -> &str {
    url.trim_end_matches(".mjs.map")
        .trim_end_matches(".mjs")
        .trim_end_matches(".json")
}

/// Cap on a fetched plugin manifest (`.json`). Manifests are small; the bound
/// stops an oversized or hostile body from being buffered in the app process.
const MAX_MANIFEST_BYTES: usize = 1024 * 1024;

/// Cap on fetched plugin code (`.mjs`). Generous for a bundled plugin; bounds
/// memory if a plugin host returns an arbitrarily large body.
const MAX_CODE_BYTES: usize = 16 * 1024 * 1024;

/// Stream `resp`'s body into memory, failing if it exceeds `max` (rejected without
/// buffering the whole body); mirrors the updater's capped download.
///
/// The one bounded body read on the plugin paths. Its callers differ only in how they decode
/// the bytes afterwards, which is why the cap sits here and not at each decode.
pub(super) async fn read_bytes_capped(
    resp: reqwest::Response,
    max: usize,
    what: &str,
) -> anyhow::Result<Vec<u8>> {
    use futures_util::StreamExt as _;
    let mut buf: Vec<u8> = Vec::new();
    let mut total = 0usize;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| anyhow::anyhow!("{what} transport failed: {e}"))?;
        total = bump_capped(total, chunk.len(), max)?;
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

/// [`read_bytes_capped`] as text, rejecting anything that is not valid UTF-8.
async fn read_body_capped(
    resp: reqwest::Response,
    max: usize,
    what: &str,
) -> anyhow::Result<String> {
    let buf = read_bytes_capped(resp, max, what).await?;
    String::from_utf8(buf).map_err(|e| anyhow::anyhow!("{what} is not valid UTF-8: {e}"))
}

/// GET `url` as text under the `max` cap; any non-2xx is an error.
async fn fetch_text_capped(
    client: &reqwest::Client,
    url: &str,
    max: usize,
    what: &str,
) -> anyhow::Result<String> {
    let resp = client.get(url).send().await?.error_for_status()?;
    read_body_capped(resp, max, what).await
}

/// Fetch a plugin's `.json` manifest. `Ok(None)` = genuinely absent (404, a
/// classic .mjs-only plugin); `Err` = a hard failure (oversized/cap, bad JSON,
/// transport, other non-2xx) that callers must surface, not fall back on.
async fn fetch_plugin_package(
    client: &reqwest::Client,
    url: &str,
) -> anyhow::Result<Option<String>> {
    let base = sanitize_plugin_url(url);
    let json_url = format!("{base}.json");
    let resp = client.get(&json_url).send().await?;
    if manifest_absent(resp.status()) {
        return Ok(None);
    }
    let resp = resp.error_for_status()?;
    let manifest_str = read_body_capped(resp, MAX_MANIFEST_BYTES, "plugin manifest").await?;
    let _: serde_json::Value = serde_json::from_str(&manifest_str)?;
    Ok(Some(manifest_str))
}

/// A plugin's manifest as fetched, and whether the source served it.
pub(super) enum Manifest {
    Served(String),
    /// Built for a source answering 404 for the `.json`: a classic `.mjs`-only plugin.
    MadeUp(String),
}

impl Manifest {
    pub(super) fn into_string(self) -> String {
        match self {
            Self::Served(text) | Self::MadeUp(text) => text,
        }
    }
}

pub(super) struct PluginData {
    pub(super) name: String,
    pub(super) manifest: Manifest,
    pub(super) code: String,
    pub(super) hash: String,
}

/// The version `url` serves. Given the `stored` version, its code is reused, not downloaded,
/// while the manifest still names it ([`serves_stored_code`]).
async fn fetch_plugin_data(
    client: &reqwest::Client,
    url: &str,
    stored: Option<&StoredPlugin>,
) -> anyhow::Result<PluginData> {
    let base = sanitize_plugin_url(url);

    // `?` propagates a hard manifest failure (cap/JSON/transport); only a
    // genuinely-absent manifest (`None`) falls back to the legacy .mjs path.
    if let Some(manifest_str) = fetch_plugin_package(client, base).await? {
        let manifest: serde_json::Value = serde_json::from_str(&manifest_str)?;
        // No "unknown" stand-in: it made every nameless plugin share one name and its data.
        let name = manifest["name"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("plugin manifest has no name"))?
            .to_string();

        let code = match stored {
            Some(stored) if serves_stored_code(&stored.manifest, &stored.code, &manifest_str) => {
                stored.code.clone()
            }
            _ => {
                let code_url = format!("{base}.mjs");
                fetch_text_capped(client, &code_url, MAX_CODE_BYTES, "plugin code").await?
            }
        };
        let hash = fnv_hash_str(&code);

        Ok(PluginData {
            name,
            manifest: Manifest::Served(manifest_str),
            code,
            hash,
        })
    } else {
        let fetch_url = if url.ends_with("package.json") {
            url.to_string()
        } else if !url.ends_with(".mjs") {
            format!("{base}.mjs")
        } else {
            url.to_string()
        };

        let code = fetch_text_capped(client, &fetch_url, MAX_CODE_BYTES, "plugin code").await?;

        let name = base
            .rsplit_once('/')
            .map(|(_, n)| n)
            .unwrap_or("plugin")
            .to_string();
        let manifest = serde_json::json!({"name": &name, "main": &fetch_url}).to_string();
        let hash = fnv_hash_str(&code);

        Ok(PluginData {
            name,
            manifest: Manifest::MadeUp(manifest),
            code,
            hash,
        })
    }
}

/// Running byte-total guard; `saturating_add` keeps a huge chunk from overflowing
/// into a pass. Mirrors the updater's download cap.
fn bump_capped(running: usize, add: usize, max: usize) -> anyhow::Result<usize> {
    let next = running.saturating_add(add);
    if next > max {
        anyhow::bail!("response exceeds the {max}-byte cap");
    }
    Ok(next)
}

/// A missing `.json` manifest (classic .mjs-only plugin) is a 404; other non-2xx
/// are real failures the caller must surface, not silently fall back on.
fn manifest_absent(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::NOT_FOUND
}

#[cfg(test)]
#[path = "../../../tests/unit/ipc/plugin/source.rs"]
mod tests;
