use super::{ipc_callback_err, ipc_callback_ok, take_ipc_callback};
use crate::app_state::{IpcCallback, IpcMessage, with_state};
use crate::plugins::store::StoredPlugin;
use std::time::Duration;

/// Bounds a version's fetches together. The source client's read timeout ends a stall, not a
/// trickle: a byte sent just inside each timeout would hold the install turn for years at the
/// code cap.
const FETCH_TIMEOUT: Duration = Duration::from_secs(300);

pub(super) fn handle_plugin_fetch_package(msg: &IpcMessage, callback: IpcCallback) {
    let url = msg.arg(0).to_string();
    crate::state::rt_handle().spawn(async move {
        let client = &*crate::state::HTTP_CLIENT_SOURCE;
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

/// The fnv of the code the source serves, fetched as `plugin.update` fetches it. While the
/// manifest still names the stored code, that is the fnv stored with it: no code is read.
async fn served_code_hash(url: &str) -> Result<String, String> {
    let key = url.to_string();
    let stored = tokio::task::spawn_blocking(move || {
        crate::state::db().call_plugins(move |pc| crate::plugins::store::stored_version(pc, &key))
    })
    .await
    .map_err(|e| format!("spawn_blocking failed: {e}"))?
    .map_err(|e| format!("DB error: {e}"))?;
    let client = &*crate::state::HTTP_CLIENT_SOURCE;
    within_fetch_timeout(async {
        // A manifest with no `hash` never spares the code: only the code tells a change.
        if let Some(stored) = &stored
            && manifest_hash(&stored.manifest).is_none()
        {
            let served = !is_legacy_manifest(&stored.manifest);
            let code = fetch_text_capped(
                client,
                &code_url(url, served),
                MAX_CODE_BYTES,
                "plugin code",
            )
            .await?;
            return Ok(fnv_hash_str(&code));
        }
        let manifest = fetch_plugin_package(client, url).await?;
        if let (Some(fetched), Some(stored)) = (&manifest, &stored)
            && let Some(hash) = &stored.hash
            && serves_stored_code(&stored.content_hash, fetched)
        {
            return Ok(hash.clone());
        }
        version_from(client, url, manifest, None)
            .await
            .map(|fetched| fetched.hash)
    })
    .await
}

/// [`fetch_plugin_data`] on the source client, within [`FETCH_TIMEOUT`].
pub(super) async fn fetch_version(
    url: &str,
    stored: Option<&StoredPlugin>,
) -> Result<PluginData, String> {
    let client = &*crate::state::HTTP_CLIENT_SOURCE;
    within_fetch_timeout(fetch_plugin_data(client, url, stored)).await
}

/// `fetch` within [`FETCH_TIMEOUT`], its failure told as a [`SourceFault`].
async fn within_fetch_timeout<T>(
    fetch: impl Future<Output = anyhow::Result<T>>,
) -> Result<T, String> {
    tokio::time::timeout(FETCH_TIMEOUT, fetch)
        .await
        .map_err(|_| SourceFault::TooSlow.to_string())?
        .map_err(|e| SourceFault::of(&e).to_string())
}

/// Why a source fetch failed, in the words the user reads.
enum SourceFault {
    /// Connected, and no response headers came.
    NoAnswer,
    /// The body stopped coming.
    StoppedSending,
    /// Still sending at [`FETCH_TIMEOUT`].
    TooSlow,
    /// Anything else, a source never reached included, told by its full error chain.
    Transport(String),
}

impl SourceFault {
    fn of(e: &anyhow::Error) -> Self {
        let Some(cause) = e.chain().find_map(|c| c.downcast_ref::<reqwest::Error>()) else {
            return Self::Transport(format!("{e:#}"));
        };
        // A connect timeout is a timeout too, but that source never answered.
        if cause.is_connect() || !cause.is_timeout() {
            Self::Transport(format!("{e:#}"))
        } else if cause.is_request() {
            Self::NoAnswer
        } else {
            Self::StoppedSending
        }
    }
}

impl std::fmt::Display for SourceFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // No number: an HTTP/2 keep-alive can end a silent source before `read_timeout`.
            Self::NoAnswer => f.write_str("The source did not answer"),
            Self::StoppedSending => f.write_str("The source stopped sending"),
            Self::TooSlow => write!(
                f,
                "The source did not finish sending within {} s",
                FETCH_TIMEOUT.as_secs()
            ),
            Self::Transport(chain) => f.write_str(chain),
        }
    }
}

/// Whether the stored code is still what the source serves: the fetched manifest's `hash` is the
/// stored code's [`content_hash`](crate::plugins::store::content_hash). A pair stored
/// mid-publish (new manifest, old code) fails it, so its code is downloaded again.
fn serves_stored_code(stored_content_hash: &str, fetched_manifest: &str) -> bool {
    manifest_hash(fetched_manifest).as_deref() == Some(stored_content_hash)
}

/// A manifest's `hash`, when it is a string: the name a Luna build gives the code beside it.
fn manifest_hash(manifest: &str) -> Option<String> {
    let manifest: serde_json::Value = serde_json::from_str(manifest).ok()?;
    manifest.get("hash")?.as_str().map(str::to_string)
}

/// Whether a stored manifest has the shape `fetch_plugin_data` makes up for a source with no
/// `.json`. Only for the stored row, which keeps no provenance: a fetch says it outright.
pub(super) fn is_legacy_manifest(manifest: &str) -> bool {
    serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(manifest)
        .is_ok_and(|keys| keys.len() == 2 && keys.contains_key("name") && keys.contains_key("main"))
}

/// Where `url`'s code is fetched: `{base}.mjs`, or `url` itself when the source serves no
/// manifest and `url` names a file.
fn code_url(url: &str, served_manifest: bool) -> String {
    let named = url.ends_with("package.json") || url.ends_with(".mjs");
    if served_manifest || !named {
        format!("{}.mjs", sanitize_plugin_url(url))
    } else {
        url.to_string()
    }
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
        let chunk = chunk.map_err(|e| {
            // A timeout stays typed: `SourceFault::of` tells a stalled body by it.
            if e.is_timeout() {
                anyhow::Error::new(e).context(format!("{what} transport failed"))
            } else {
                anyhow::anyhow!("{what} transport failed: {e}")
            }
        })?;
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
    // `?` propagates a hard manifest failure (cap/JSON/transport); only a
    // genuinely-absent manifest (`None`) falls back to the legacy .mjs path.
    let manifest = fetch_plugin_package(client, url).await?;
    version_from(client, url, manifest, stored).await
}

/// [`fetch_plugin_data`] once the manifest is fetched.
async fn version_from(
    client: &reqwest::Client,
    url: &str,
    manifest: Option<String>,
    stored: Option<&StoredPlugin>,
) -> anyhow::Result<PluginData> {
    let base = sanitize_plugin_url(url);

    if let Some(manifest_str) = manifest {
        let manifest: serde_json::Value = serde_json::from_str(&manifest_str)?;
        // No "unknown" stand-in: it made every nameless plugin share one name and its data.
        let name = manifest["name"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("plugin manifest has no name"))?
            .to_string();

        let code = match stored {
            Some(stored) if serves_stored_code(&stored.content_hash, &manifest_str) => {
                stored.code.clone()
            }
            _ => {
                fetch_text_capped(client, &code_url(url, true), MAX_CODE_BYTES, "plugin code")
                    .await?
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
        let fetch_url = code_url(url, false);
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
