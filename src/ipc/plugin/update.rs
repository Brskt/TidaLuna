//! Bringing an installed plugin to the version its url serves now: `plugin.update` stores it for
//! the Reload button and the live-reload poll, which restart the plugin themselves;
//! `plugin.update_all` stores and restarts once per launch, behind its setting.

use super::inject::PassDuty;
use super::ipc_callback_ok;
use super::plugin_ipc::{Manifest, PluginData};
use crate::app_state::{IpcCallback, IpcMessage, with_state};
use crate::plugins::store::UpdateOutcome;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
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

/// Set once the launch check has run; a later page reload does not run it again.
static LAUNCH_CHECK_RAN: AtomicBool = AtomicBool::new(false);

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

/// What the launch check made of a stored update.
enum Restarted {
    Yes,
    /// Not running: the stored version is the one its next enable or launch loads.
    NotLive,
    /// Left running as it was, for the reason given; the new version loads at the next launch.
    Deferred(String),
    /// The new code failed to load and its flag was written off, as any failed load does.
    Disabled(String),
    /// The old load is gone and nothing new runs, but the flag stays as the user set it: the
    /// failure was this machine's. The next launch loads the stored version.
    Stopped(String),
    /// The session ended under the restart: nothing is owed and nothing may be written or said.
    SessionEnded,
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

/// Answers at once: the check runs on its own, each plugin in its own task, and what changes
/// reaches the page as `jsrt.plugin_updated`. Without a session nothing runs and the launch keeps
/// its check for the page that follows the login.
pub(super) fn handle_plugin_update_all(callback: IpcCallback) {
    ipc_callback_ok(&callback, "true");
    if !super::plugin_ipc::has_session() || LAUNCH_CHECK_RAN.swap(true, Ordering::SeqCst) {
        return;
    }
    crate::state::rt_handle().spawn(async {
        let read = tokio::task::spawn_blocking(|| {
            crate::state::db().call(|pc, sc| {
                if !crate::settings::load_plugin_update_at_boot(sc) {
                    return Vec::new();
                }
                crate::plugins::store::enabled_urls(pc)
            })
        })
        .await;
        let urls: Vec<String> = read.unwrap_or_default();
        // A plugin served from this machine is one being written: replacing it at each launch
        // would undo the developer's own reloads.
        for url in urls.into_iter().filter(|url| !is_loopback(url)) {
            crate::state::rt_handle().spawn(launch_check(url));
        }
    });
}

/// One plugin of the launch check. The restart runs under the same turn as the store: a Reload
/// clicked meanwhile finds the plugin busy instead of restarting it alongside.
async fn launch_check(url: String) {
    let Some(_turn) = Turn::take(&url) else {
        return;
    };
    match pull(&url).await {
        Pulled::Updated { .. } => match restart_live(&url).await {
            Restarted::Yes | Restarted::NotLive => {}
            Restarted::Deferred(why) => {
                crate::vprintln!("[PLUGIN] '{url}' runs its update from the next launch: {why}");
            }
            Restarted::Disabled(why) => {
                crate::vprintln!("[PLUGIN] '{url}' did not restart on its update: {why}");
                let reason = format!("The plugin did not restart on its update: {why}");
                crate::app_state::emit_ipc_event_with_args(
                    "jsrt.plugin_failed",
                    &[url.as_str(), reason.as_str()],
                );
            }
            Restarted::Stopped(why) => {
                crate::vprintln!("[PLUGIN] '{url}' did not restart on its update: {why}");
                let reason = format!("The plugin did not restart on its update: {why}");
                crate::app_state::emit_ipc_event_with_args(
                    "jsrt.plugin_stopped",
                    &[url.as_str(), reason.as_str()],
                );
            }
            Restarted::SessionEnded => {
                crate::vprintln!("[PLUGIN] '{url}' restart abandoned: the session ended");
            }
        },
        Pulled::Refused { reason, .. }
        | Pulled::Blocked { reason }
        | Pulled::Unavailable { reason } => {
            crate::vprintln!("[PLUGIN] '{url}' not updated: {reason}");
        }
        Pulled::UpToDate { .. } | Pulled::Busy | Pulled::Gone => {}
    }
}

/// Every way a url names this machine: `localhost` and its subdomains (RFC 6761), loopback and
/// unspecified addresses, IPv4 ones written as IPv6.
fn is_loopback(url: &str) -> bool {
    use url::Host;
    url::Url::parse(url).is_ok_and(|parsed| match parsed.host() {
        Some(Host::Domain(domain)) => {
            let domain = domain.trim_end_matches('.').to_ascii_lowercase();
            domain == "localhost" || domain.ends_with(".localhost")
        }
        Some(Host::Ipv4(ip)) => ip.is_loopback() || ip.is_unspecified(),
        Some(Host::Ipv6(ip)) => match ip.to_ipv4_mapped() {
            Some(v4) => v4.is_loopback() || v4.is_unspecified(),
            None => ip.is_loopback() || ip.is_unspecified(),
        },
        None => false,
    })
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

/// Puts the stored version in place of the plugin's live load. `enabled` stays as the user set it
/// unless the new code itself fails to load, which turns it off as any failed load does.
async fn restart_live(url: &str) -> Restarted {
    if with_state(|state| state.plugin_manager.current_load_id(url))
        .flatten()
        .is_none()
    {
        return Restarted::NotLive;
    }
    let read = {
        let url = url.to_string();
        blocking(move || {
            crate::state::db().call_plugins(move |pc| crate::plugins::store::stored(pc, &url))
        })
        .await
    };
    let stored = match read {
        Ok(Ok(Some(stored))) => stored,
        Ok(Ok(None)) => return Restarted::NotLive,
        Ok(Err(e)) => return Restarted::Deferred(format!("DB error: {e}")),
        Err(why) => return Restarted::Deferred(why),
    };
    let Ok(name) = crate::plugins::name::PluginName::parse(&stored.name) else {
        return Restarted::Deferred(format!("'{}' is not a plugin name", stored.name));
    };
    // Enable and disable are not serialized against this: the flag is read before the old load
    // goes, and again once the new one is in, where a disable that landed between them undoes it.
    match restart_check(url, &name).await {
        Ok(true) => {}
        Ok(false) => return Restarted::NotLive,
        Err(why) => return Restarted::Deferred(why),
    }
    // Read late, as a disable does: the check above is a real await.
    let Some(old) = with_state(|state| state.plugin_manager.current_load_id(url)).flatten() else {
        return Restarted::NotLive;
    };
    let epoch = with_state(|state| state.session_epoch).unwrap_or_default();
    super::inject::undo_injection(url, old);
    let injected = {
        let (url, name, code) = (url.to_string(), name.clone(), stored.code);
        blocking(move || super::inject::inject_plugin(&url, &name, &code, epoch)).await
    };
    let load_id = match injected {
        Ok(Ok(load_id)) => load_id,
        Ok(Err(failure)) => match failure.pass_duty() {
            // The plugin's own failure turns it off as a failed load does.
            PassDuty::Reconcile => {
                let disable = url.to_string();
                let _ = blocking(move || {
                    crate::state::db()
                        .call_plugins(move |pc| crate::plugins::store::disable_failed(pc, &disable))
                })
                .await;
                return Restarted::Disabled(failure.reason());
            }
            // A machine's failure leaves the flag as the user set it.
            PassDuty::Keep => return Restarted::Stopped(failure.reason()),
            PassDuty::Abandon => return Restarted::SessionEnded,
        },
        Err(why) => return Restarted::Stopped(why),
    };
    if let Ok(false) = restart_check(url, &name).await {
        super::inject::undo_injection(url, load_id);
        return Restarted::NotLive;
    }
    crate::state::rt_handle().spawn(super::inject::watch_ack(url.to_string(), load_id));
    Restarted::Yes
}

/// Whether the plugin may restart now: `Ok(false)` once the user turned it off, `Err` while
/// enabled plugins depend on it, which a disable refuses too, or while the flag cannot be read.
async fn restart_check(url: &str, name: &crate::plugins::name::PluginName) -> Result<bool, String> {
    let (url, name) = (url.to_string(), name.to_string());
    blocking(move || {
        crate::state::db().call_plugins(move |pc| {
            if !crate::plugins::store::is_enabled(pc, &url).map_err(|e| format!("DB error: {e}"))? {
                return Ok(false);
            }
            match crate::plugins::store::find_enabled_dependants(pc, &name)? {
                deps if deps.is_empty() => Ok(true),
                deps => Err(format!("enabled plugins depend on it: {}", deps.join(", "))),
            }
        })
    })
    .await?
}

async fn blocking<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> Result<R, String> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| format!("spawn_blocking failed: {e}"))
}

#[cfg(test)]
#[path = "../../../tests/unit/ipc/plugin/update.rs"]
mod tests;
