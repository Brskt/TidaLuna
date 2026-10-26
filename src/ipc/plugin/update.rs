//! Bringing an installed plugin to the version its url serves now: `plugin.update` stores it for
//! the Reload button and the live-reload poll, which restart the plugin themselves.

use super::ipc_callback_ok;
use super::plugin_ipc::{Manifest, PluginData};
use crate::app_state::{IpcCallback, IpcMessage};
use crate::plugins::store::UpdateOutcome;
use std::collections::HashSet;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

/// Bounds the two fetches together: the shared client has no read timeout, and a source stalling
/// its body would hold the plugin's update turn for good.
const FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// The urls with an update in flight. A second update of one url is refused, never queued.
static UPDATING: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Default::default);

struct Turn(String);

impl Turn {
    fn take(url: &str) -> Option<Turn> {
        let mut updating = UPDATING.lock().unwrap_or_else(|e| e.into_inner());
        updating
            .insert(url.to_string())
            .then(|| Turn(url.to_string()))
    }
}

impl Drop for Turn {
    fn drop(&mut self) {
        let mut updating = UPDATING.lock().unwrap_or_else(|e| e.into_inner());
        updating.remove(&self.0);
    }
}

/// What `plugin.update` found at the source and did with it. Named, not coded: the poll restarts,
/// retries or gives up on the kind, and one error code stood for kinds that need opposite answers.
/// `hash` is the code's, the one `plugin.check_hash` reports.
#[derive(serde::Serialize)]
#[serde(tag = "outcome", rename_all = "camelCase")]
enum Pulled {
    /// A new version is stored; the plugin still runs the old one until restarted.
    Updated {
        hash: String,
        manifest: String,
    },
    UpToDate {
        hash: String,
    },
    /// The source's version is refused, and will be until the source changes.
    Refused {
        hash: String,
        reason: String,
    },
    /// Refused for a reason outside the source (its dependencies are not enabled): worth asking again.
    Blocked {
        reason: String,
    },
    /// The source could not be read, or this machine could not take the version: worth asking again.
    Unavailable {
        reason: String,
    },
    /// Another update of this plugin is running.
    Busy,
    Gone,
}

pub(super) fn handle_plugin_update(msg: &IpcMessage, callback: IpcCallback) {
    let url = msg.arg(0).to_string();
    crate::state::rt_handle().spawn(async move {
        let pulled = match Turn::take(&url) {
            Some(_turn) => pull(&url).await,
            None => Pulled::Busy,
        };
        let json = serde_json::to_string(&pulled).unwrap_or_else(|_| "null".to_string());
        ipc_callback_ok(&callback, &json);
    });
}

/// Whether a stored manifest has the shape `fetch_plugin_data` makes up for a source with no
/// `.json`. Only for the stored row, which keeps no provenance: a fetch says it outright.
fn is_legacy_manifest(manifest: &str) -> bool {
    serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(manifest)
        .is_ok_and(|keys| keys.len() == 2 && keys.contains_key("name") && keys.contains_key("main"))
}

impl Manifest {
    /// A made-up manifest replaces only a made-up one: a `.json` missing for a moment must not
    /// drop a real manifest's dependencies and type.
    fn replaces(&self, stored: &str) -> bool {
        match self {
            Manifest::Served(_) => true,
            Manifest::MadeUp(_) => is_legacy_manifest(stored),
        }
    }
}

/// Stores the version the source serves if it may replace the stored one. Restarts nothing: the
/// caller holds the plugin's turn and decides.
async fn pull(url: &str) -> Pulled {
    let read = {
        let url = url.to_string();
        blocking(move || {
            crate::state::db().call_plugins(move |pc| crate::plugins::store::stored(pc, &url))
        })
        .await
    };
    let stored = match read {
        Ok(Ok(Some(stored))) => stored,
        Ok(Ok(None)) => return Pulled::Gone,
        Ok(Err(e)) => return unavailable(format!("DB error: {e}")),
        Err(why) => return unavailable(why),
    };
    let PluginData {
        name,
        manifest,
        code,
        hash,
    } = match fetch(url, &stored.manifest).await {
        Ok(fetched) => fetched,
        Err(why) => return unavailable(why),
    };
    let manifest = manifest.into_string();
    if manifest == stored.manifest && code == stored.code {
        return Pulled::UpToDate { hash };
    }
    if let Err(refused) = crate::plugins::name::PluginName::parse(&name) {
        return refused_as(hash, refused.to_string());
    }
    if let Err(why) = super::plugin_ipc::check_luna_type(&manifest) {
        return refused_as(hash, why);
    }
    let filename = format!("{url}.mts");
    let source = code.clone();
    match blocking(move || crate::plugins::transpile_helper::transpile(&source, &filename)).await {
        Ok(Ok(_)) => {}
        Ok(Err(crate::plugins::transpile_helper::Failure::Source(e))) => {
            return refused_as(hash, format!("The new version does not compile: {e}"));
        }
        Ok(Err(crate::plugins::transpile_helper::Failure::Helper(e))) => {
            return unavailable(format!("Cannot prepare plugins: {e}"));
        }
        Err(why) => return unavailable(why),
    }
    let outcome = {
        let (url, name, manifest, hash) = (
            url.to_string(),
            name.clone(),
            manifest.clone(),
            hash.clone(),
        );
        blocking(move || {
            crate::state::db().call_plugins(move |pc| {
                crate::plugins::store::update(pc, &url, &name, &manifest, &code, &hash)
            })
        })
        .await
    };
    match outcome {
        Ok(Ok(UpdateOutcome::Updated)) => {
            crate::vprintln!("[PLUGIN] Updated '{name}' from its source");
            crate::app_state::emit_ipc_event_with_args("jsrt.plugin_updated", &[url]);
            Pulled::Updated { hash, manifest }
        }
        Ok(Ok(UpdateOutcome::UpToDate)) => Pulled::UpToDate { hash },
        Ok(Ok(UpdateOutcome::Renamed { stored })) => refused_as(
            hash,
            format!("The source renamed '{stored}' to '{name}': reinstall it"),
        ),
        Ok(Ok(UpdateOutcome::Gone)) => Pulled::Gone,
        Ok(Ok(UpdateOutcome::MissingDependencies(missing))) => Pulled::Blocked {
            reason: format!("The new version needs: {}", missing.join(", ")),
        },
        Ok(Err(e)) => unavailable(format!("DB error: {e}")),
        Err(why) => unavailable(why),
    }
}

fn unavailable(reason: String) -> Pulled {
    Pulled::Unavailable { reason }
}

fn refused_as(hash: String, reason: String) -> Pulled {
    Pulled::Refused { hash, reason }
}

/// The version the source serves. A source that lost its `.json` is an error, not a plugin
/// without one: `fetch_plugin_data` would make up a manifest and drop the real one.
async fn fetch(url: &str, stored_manifest: &str) -> Result<PluginData, String> {
    let client = &*crate::state::HTTP_CLIENT;
    let fetched = tokio::time::timeout(
        FETCH_TIMEOUT,
        super::plugin_ipc::fetch_plugin_data(client, url),
    )
    .await
    .map_err(|_| {
        format!(
            "The source did not answer within {} s",
            FETCH_TIMEOUT.as_secs()
        )
    })?
    .map_err(|e| format!("{e:#}"))?;
    if !fetched.manifest.replaces(stored_manifest) {
        return Err("The source no longer serves the plugin's manifest".to_string());
    }
    Ok(fetched)
}

async fn blocking<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> Result<R, String> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| format!("spawn_blocking failed: {e}"))
}

#[cfg(test)]
#[path = "../../../tests/unit/ipc/plugin/update.rs"]
mod tests;
