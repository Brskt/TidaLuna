use super::{ipc_callback_err, ipc_callback_ok};
use crate::app_state::{IpcCallback, IpcMessage};
use crate::native_runtime::NativeRuntime;
use crate::state::{NATIVE_RUNTIME, NATIVE_RUNTIME_INIT};

use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use tokio::sync::watch;

/// Network modules are granted as a group - one dialog covers all of them.
const NETWORK_MODULES: &[&str] = &[
    "net",
    "http",
    "https",
    "http2",
    "tls",
    "dns",
    "dns/promises",
];

/// Pending trust requests: keyed by "name::module".
/// Watch channels let concurrent register calls for the same plugin share a
/// single dialog prompt.
type TrustMap = HashMap<String, watch::Sender<Option<bool>>>;

static PENDING_TRUST: LazyLock<Mutex<TrustMap>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Every native module this process holds or is acquiring, keyed by module name.
///
/// One ledger under one lock: splitting settled and in-flight state let a registration settle between
/// the two reads and pass the bound, let concurrent attempts on one module retire each other, and
/// double-counted a re-registration into a spurious 403. Keying by module makes counting a scan of
/// distinct entries: none of the three is expressible.
static NATIVE_MODULES: LazyLock<Mutex<HashMap<String, ModuleEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Default)]
struct ModuleEntry {
    /// Attempts in flight for this module. The entry outlives every one of them; no attempt can
    /// retire a module another is still acquiring.
    in_flight: usize,
    /// The channel once Bun has answered, `None` while the module is only being acquired.
    channel: Option<Channel>,
}

struct Channel {
    /// The token that reaches this module. The channel used to be `__LunaNative.{name}`, derivable
    /// from the module name, and the call path checks no ownership. Rust returns the channel as the
    /// register result. Plugin code never sees the difference.
    token: String,
    code_hash: String,
}

/// `None` if the RNG is unavailable: fail closed rather than fall back to a guessable channel.
///
/// Same name and hash reuses the token. Trust is keyed by `code_hash`, and minting a fresh one
/// evicted a concurrent caller's. Different code replaces it, since Bun's `modules[name]` is one
/// mutable slot with no caller identity of its own.
fn issue_native_channel(name: &str, code_hash: &str) -> Option<String> {
    let mut modules = NATIVE_MODULES.lock().unwrap_or_else(|e| e.into_inner());
    let entry = modules.entry(name.to_string()).or_default();
    if let Some(channel) = entry.channel.as_ref().filter(|c| c.code_hash == code_hash) {
        return Some(channel.token.clone());
    }
    let token = crate::plugins::manager::random_capability()?;
    entry.channel = Some(Channel {
        token: token.clone(),
        code_hash: code_hash.to_string(),
    });
    Some(token)
}

/// Distinct modules one plugin may hold at once. The name is caller-chosen and every registration
/// also loads a module into the Bun child; without a bound, one plugin grows both this ledger and
/// that child without limit. A plugin ships a fixed, tiny set of `.native.ts` files.
const MAX_MODULES_PER_PLUGIN: usize = 8;

/// One attempt's claim on a module, held from dispatch until it settles. Released by `Drop`. No exit
/// path (Bun error, denied trust, dropped response channel) can leak one.
struct PendingRegistration(String);

impl PendingRegistration {
    /// Claim `name` for this attempt, or `None` if the plugin is at the bound.
    ///
    /// A module the plugin already holds or is already acquiring needs no new slot; anything else
    /// counts against the bound. One lock for the whole decision leaves nothing to settle underneath.
    fn reserve(name: &str, plugin_prefix: &str) -> Option<Self> {
        let mut modules = NATIVE_MODULES.lock().unwrap_or_else(|e| e.into_inner());
        if !modules.contains_key(name)
            && !admits_new_module(modules.keys().map(String::as_str), plugin_prefix)
        {
            return None;
        }
        modules.entry(name.to_string()).or_default().in_flight += 1;
        Some(Self(name.to_string()))
    }

    /// The module this attempt is for. The attempt reads its name from here rather than keeping a
    /// second copy: one owner, and nothing can drift between them.
    fn module(&self) -> &str {
        &self.0
    }
}

impl Drop for PendingRegistration {
    fn drop(&mut self) {
        let mut modules = NATIVE_MODULES.lock().unwrap_or_else(|e| e.into_inner());
        let Some(entry) = modules.get_mut(&self.0) else {
            // Uninstall cleared the entry while this attempt was in flight.
            return;
        };
        entry.in_flight = entry.in_flight.saturating_sub(1);
        // An attempt that never produced a channel leaves nothing behind; the slot goes back.
        if entry.in_flight == 0 && entry.channel.is_none() {
            modules.remove(&self.0);
        }
    }
}

/// Has this plugin room for one more distinct module? Counts the ledger's own entries, unique by
/// construction: no module is counted twice however many attempts it has in flight. Refuses rather
/// than evicting the oldest: an evicted token unreaches a module but does not unload it from Bun;
/// eviction would bound this ledger while the child kept growing. Pure, and testable off the
/// process-global the other tests share.
fn admits_new_module<'a>(held: impl Iterator<Item = &'a str>, plugin_prefix: &str) -> bool {
    held.filter(|module| module_belongs_to(module, plugin_prefix))
        .count()
        < MAX_MODULES_PER_PLUGIN
}

/// An empty token never resolves; a bare `__LunaNative.` cannot match. Scanned rather than indexed
/// by token: the ledger holds at most `MAX_MODULES_PER_PLUGIN` per plugin. A scan per call is
/// cheaper than keeping a second token-to-module index in step.
fn module_for_native_channel(token: &str) -> Option<String> {
    if token.is_empty() {
        return None;
    }
    NATIVE_MODULES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|(_, entry)| entry.channel.as_ref().is_some_and(|c| c.token == token))
        .map(|(module, _)| module.clone())
}

/// Clear in-memory watch channels for a plugin (called on uninstall).
/// Keys are "name::module"; we remove any key starting with the plugin prefix.
pub(super) fn clear_pending_trust(plugin_prefix: &str) {
    let mut guard = PENDING_TRUST.lock().unwrap_or_else(|e| e.into_inner());
    guard.retain(|key, _| !key.starts_with(plugin_prefix));
}

/// Drop a plugin's channel tokens on uninstall, which revokes its persisted trust: a leftover
/// renderer closure would otherwise keep invoking exports whose grants were just withdrawn. Not done
/// on disable, where the cleanup handler still runs after the unload is dispatched.
pub(super) fn clear_native_channels(plugin_prefix: &str) {
    NATIVE_MODULES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|module, _| !module_belongs_to(module, plugin_prefix));
}

/// An empty prefix owns everything, the convention `clear_pending_trust` and `clear_trust_by_plugin`
/// follow and what `plugin.uninstall_all` passes. Kept out of the map: testable without clearing a
/// process-global the other tests share.
fn module_belongs_to(module: &str, plugin_prefix: &str) -> bool {
    if plugin_prefix.is_empty() {
        return true;
    }
    // Names are "{plugin}/{file}.native.ts"; the separator stops "foo" matching "foobar". Split
    // rather than formatted: this runs once per held module on every registration and every uninstall.
    module
        .strip_prefix(plugin_prefix)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

fn compute_code_hash(code: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(code.trim().as_bytes());
    base16ct::lower::encode_string(&hasher.finalize())
}

/// Initialize the native runtime (Bun child process) if not already running.
fn ensure_native_runtime() -> Result<&'static NativeRuntime, String> {
    if let Some(rt) = NATIVE_RUNTIME.get() {
        return Ok(rt);
    }
    let _guard = NATIVE_RUNTIME_INIT
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(rt) = NATIVE_RUNTIME.get() {
        return Ok(rt);
    }
    let rt = NativeRuntime::spawn(crate::state::rt_handle())
        .map_err(|e| format!("Failed to start native runtime: {e}"))?;
    crate::vprintln!("[NATIVE] Bun process started");
    if NATIVE_RUNTIME.set(rt).is_err() {
        panic!("NATIVE_RUNTIME already initialized under init lock");
    }
    Ok(NATIVE_RUNTIME.get().unwrap())
}

pub(super) fn handle_register_native(msg: &IpcMessage, callback: IpcCallback) {
    let name = msg
        .args
        .first()
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let code = msg
        .args
        .get(1)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if name.is_empty() || code.is_empty() {
        ipc_callback_err(&callback, 400, "registerNative: missing name or code");
        return;
    }

    // "DiscordRPC/discord.native.ts" -> "DiscordRPC"
    // "@scope/pkg/foo.native.ts" -> "@scope/pkg"
    let plugin_prefix = name
        .rsplit_once('/')
        .map(|(p, _)| p)
        .unwrap_or(&name)
        .to_string();

    // Confirms only that some plugin bears this name, not that the caller is it; `registerNative`
    // arrives through the shared `@luna/lib` with no capability attached. Known gap, closable only
    // by a per-plugin `@luna/lib`: it hands out another plugin's `data_dir`, and byte-identical code
    // inherits another plugin's grants (different code does not; `load_trust` filters on both).
    // The load id is captured here rather than re-derived later, because it must answer "still this
    // load" after the awaits below; it is `Some` exactly when the plugin is loaded.
    let live = crate::app_state::with_state(|state| {
        let url = state
            .plugin_manager
            .url_for_name(&plugin_prefix)?
            .to_string();
        let load_id = state.plugin_manager.current_load_id(&url)?;
        Some((url, load_id))
    })
    .flatten();
    let Some((plugin_url, load_id)) = live else {
        crate::vprintln!(
            "[NATIVE] Rejected registerNative for '{}': plugin not active",
            name
        );
        ipc_callback_err(
            &callback,
            500,
            &format!("registerNative: plugin '{}' is not active", plugin_prefix),
        );
        return;
    };

    // Reserved before Bun is touched; a caller looping fresh module names cannot grow its module
    // table. Held for the whole registration since the ledger only writes once Bun answers;
    // unawaited concurrent calls would otherwise all pass the check first.
    let Some(reservation) = PendingRegistration::reserve(&name, &plugin_prefix) else {
        crate::vprintln!(
            "[NATIVE] Refused registerNative for '{name}': at the {MAX_MODULES_PER_PLUGIN} module bound"
        );
        ipc_callback_err(
            &callback,
            403,
            &format!(
                "registerNative: '{plugin_prefix}' already holds {MAX_MODULES_PER_PLUGIN} modules"
            ),
        );
        return;
    };

    crate::vprintln!(
        "[NATIVE] Registering module '{}' ({} bytes)",
        name,
        code.len()
    );

    let runtime = match ensure_native_runtime() {
        Ok(rt) => rt,
        Err(e) => {
            ipc_callback_err(&callback, 500, &e);
            return;
        }
    };

    let code_hash = compute_code_hash(&code);
    crate::vprintln!(
        "[NATIVE] Code hash for '{}': {} (trimmed len={})",
        name,
        &code_hash[..16],
        code.trim().len()
    );

    let manifest_json: String = crate::state::db().call_plugins({
        let prefix = plugin_prefix.clone();
        move |pc| {
            pc.query_row(
                "SELECT manifest FROM plugins WHERE name = ?1 AND installed = 1",
                rusqlite::params![prefix],
                |row| row.get(0),
            )
            .unwrap_or_default()
        }
    });

    // Reject path traversal in plugin names (malicious manifest with "../" in name)
    if plugin_prefix.contains("..") {
        ipc_callback_err(&callback, 400, "registerNative: invalid plugin name");
        return;
    }
    let native_base = crate::state::cache_data_dir().join("native");
    let data_dir_path = native_base.join(&plugin_prefix);
    if !data_dir_path.starts_with(&native_base) {
        ipc_callback_err(&callback, 400, "registerNative: invalid plugin name");
        return;
    }
    let data_dir = data_dir_path.to_string_lossy().to_string();

    let trust_grants: HashMap<String, bool> = {
        let decisions = crate::state::db().call_settings({
            let plugin = name.clone();
            let code_hash = code_hash.clone();
            move |conn| crate::native_runtime::trust::load_trust(conn, &plugin, &code_hash)
        });
        decisions
            .into_iter()
            .map(|d| (d.module, d.granted))
            .collect()
    };

    do_register(
        runtime,
        RegisterAttempt {
            code,
            code_hash,
            trust_grants,
            manifest_json,
            data_dir,
            plugin_url,
            load_id,
            reservation,
        },
        callback,
    );
}

/// One registration attempt. These values all travel together through the trust retry, and the
/// reservation has to travel with them: the slot is held for the whole attempt chain, not per call.
struct RegisterAttempt {
    code: String,
    code_hash: String,
    trust_grants: HashMap<String, bool>,
    manifest_json: String,
    data_dir: String,
    /// The plugin this attempt was admitted for, and which load of it. Re-read at the mint, since
    /// admission happened before an unbounded wait.
    plugin_url: String,
    load_id: u64,
    /// Held from dispatch until the attempt settles, then released by `Drop`. Also the owner of the
    /// module name, read through `module()`.
    reservation: PendingRegistration,
}

/// Send register command to Bun, handle TRUST_REQUIRED sentinel.
fn do_register(
    runtime: &'static NativeRuntime,
    mut attempt: RegisterAttempt,
    callback: IpcCallback,
) {
    let trust_json: serde_json::Value = if attempt.trust_grants.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::json!(attempt.trust_grants)
    };
    let cmd = serde_json::json!({
        "type": "register",
        "name": attempt.reservation.module(),
        "code": attempt.code,
        "trust": trust_json,
        "dataDir": attempt.data_dir,
    });
    let rx = match runtime.send_command(cmd) {
        Ok(rx) => rx,
        Err(e) => {
            ipc_callback_err(&callback, 500, &e);
            return;
        }
    };

    crate::state::rt_handle().spawn(async move {
        match rx.await {
            Ok(Ok(response)) => {
                let exports = response
                    .get("exports")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                // Admission can be arbitrarily stale (a trust dialog may sit open indefinitely).
                // Re-check before minting: `issue_native_channel` inserts a missing entry; it would
                // otherwise put back the one `clear_native_channels` just removed and hand a working
                // token to an uninstalled plugin, which Bun never unloads. The load id also catches a
                // disable-then-enable, where the plugin is back but this is not its registration.
                let same_load = crate::app_state::with_state(|state| {
                    state.plugin_manager.current_load_id(&attempt.plugin_url) == Some(attempt.load_id)
                })
                .unwrap_or(false);
                if !same_load {
                    crate::vprintln!(
                        "[NATIVE] Dropped '{}': the plugin went away while the registration was in flight",
                        attempt.reservation.module()
                    );
                    clear_pending_trust(attempt.reservation.module());
                    ipc_callback_err(&callback, 500, "registerNative: plugin is no longer active");
                    return;
                }
                let Some(token) =
                    issue_native_channel(attempt.reservation.module(), &attempt.code_hash)
                else {
                    clear_pending_trust(attempt.reservation.module());
                    ipc_callback_err(&callback, 500, "RNG unavailable");
                    return;
                };
                let channel = format!("__LunaNative.{token}");
                crate::vprintln!(
                    "[NATIVE] Registered '{}': {} exports ({})",
                    attempt.reservation.module(),
                    exports.len(),
                    exports.join(", ")
                );
                clear_pending_trust(attempt.reservation.module());
                ipc_callback_ok(&callback, &format!("\"{channel}\""));
            }
            Ok(Err(e)) => {
                if let Some(raw) = e.strip_prefix("TRUST_REQUIRED:") {
                    // Trim stack trace - only keep the module name (first line, no whitespace)
                    let module = raw.lines().next().unwrap_or(raw).trim().to_string();

                    if attempt.trust_grants.get(&module) == Some(&false) {
                        crate::vprintln!(
                            "[NATIVE] Trust previously denied for '{}' -> module '{}'",
                            attempt.reservation.module(),
                            module
                        );
                        ipc_callback_err(
                            &callback,
                            403,
                            &format!(
                                "Plugin '{}' denied access to module '{}' (persisted)",
                                attempt.reservation.module(),
                                module
                            ),
                        );
                        return;
                    }

                    crate::vprintln!(
                        "[NATIVE] Trust required for '{}' -> module '{}'",
                        attempt.reservation.module(),
                        module
                    );

                    // Dedup: if a dialog is already pending for this module,
                    // subscribe to the same watch channel - no duplicate popup.
                    let trust_key = format!("{}::{}", attempt.reservation.module(), module);
                    let mut rx = {
                        let mut guard = PENDING_TRUST.lock().unwrap_or_else(|e| e.into_inner());
                        if let Some(existing_tx) = guard.get(&trust_key) {
                            existing_tx.subscribe()
                        } else {
                            let (tx, sub_rx) = watch::channel(None);
                            guard.insert(trust_key.clone(), tx);
                            drop(guard);

                            let dialog_key = trust_key.clone();
                            let dialog_rx = crate::ui::trust_dialog::show_trust_dialog(
                                attempt.reservation.module(),
                                &module,
                                &attempt.manifest_json,
                            );
                            // Broadcast result without removing the key - late
                            // subscribers can still dedup via rx.borrow(). The key
                            // is removed after save_trust makes the decision durable.
                            crate::state::rt_handle().spawn(async move {
                                let granted = dialog_rx.await.unwrap_or(false);
                                let guard = PENDING_TRUST.lock().unwrap_or_else(|e| e.into_inner());
                                if let Some(tx) = guard.get(&dialog_key) {
                                    let _ = tx.send(Some(granted));
                                }
                            });
                            sub_rx
                        }
                    };

                    // Wait for the dialog result.
                    // Check current value first - a late subscriber may find the
                    // answer already set by an earlier concurrent register call.
                    let granted = if let Some(val) = *rx.borrow() {
                        val
                    } else {
                        loop {
                            if rx.changed().await.is_err() {
                                break false;
                            }
                            if let Some(val) = *rx.borrow() {
                                break val;
                            }
                        }
                    };

                    let is_net = NETWORK_MODULES.contains(&module.as_str());

                    // Persist trust/denial (entire network group if applicable)
                    {
                        let hash = attempt.code_hash.clone();
                        let plugin = attempt.reservation.module().to_string();
                        let mod_name = module.clone();
                        crate::state::db().call_settings(move |conn| {
                            if is_net {
                                for &m in NETWORK_MODULES {
                                    if let Err(e) = crate::native_runtime::trust::save_trust(
                                        conn, &hash, &plugin, m, granted,
                                    ) {
                                        crate::vprintln!(
                                            "[NATIVE] Failed to save trust for {}::{}: {}",
                                            plugin,
                                            m,
                                            e
                                        );
                                    }
                                }
                            } else if let Err(e) = crate::native_runtime::trust::save_trust(
                                conn, &hash, &plugin, &mod_name, granted,
                            ) {
                                crate::vprintln!(
                                    "[NATIVE] Failed to save trust for {}::{}: {}",
                                    plugin,
                                    mod_name,
                                    e
                                );
                            }
                        });
                    }
                    // DB is durable - safe to remove the dedup key now.
                    {
                        let mut guard = PENDING_TRUST.lock().unwrap_or_else(|e| e.into_inner());
                        guard.remove(&trust_key);
                    }

                    if !granted {
                        crate::vprintln!(
                            "[NATIVE] Trust denied for '{}' -> module '{}'",
                            attempt.reservation.module(),
                            module
                        );
                        ipc_callback_err(
                            &callback,
                            403,
                            &format!(
                                "Plugin '{}' denied access to module '{}'",
                                attempt.reservation.module(),
                                module
                            ),
                        );
                        return;
                    }

                    // Grant in-memory (entire network group if applicable)
                    if is_net {
                        for &m in NETWORK_MODULES {
                            attempt.trust_grants.insert(m.to_string(), true);
                        }
                    } else {
                        attempt.trust_grants.insert(module, true);
                    }
                    // The attempt's reservation travels with it, keeping the retry on the same slot.
                    do_register(runtime, attempt, callback);
                    return;
                }

                crate::vprintln!(
                    "[NATIVE] Register failed for '{}': {}",
                    attempt.reservation.module(),
                    e
                );
                ipc_callback_err(&callback, 500, &e);
            }
            Err(_) => {
                ipc_callback_err(&callback, 500, "Bun response channel dropped");
            }
        }
    });
}

/// Handle `__LunaNative.{name}` IPC calls to a registered native module.
pub(super) fn handle_native_call(msg: &IpcMessage, callback: IpcCallback) {
    // The channel carries the token Rust issued at registration, not the module name: a
    // plugin cannot reach a module it did not register by naming it.
    let token = msg.channel.strip_prefix("__LunaNative.").unwrap_or("");
    let Some(module_name) = module_for_native_channel(token) else {
        // Gated, and without the token: the caller is told, and the token must not reach the log.
        crate::vprintln!("[NATIVE] Refused a call on an unissued channel");
        ipc_callback_err(&callback, 403, "unknown native channel");
        return;
    };
    let export_name = msg
        .args
        .first()
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let call_args: Vec<serde_json::Value> = msg.args.iter().skip(1).cloned().collect();

    let Some(runtime) = NATIVE_RUNTIME.get() else {
        ipc_callback_err(&callback, 500, "Native runtime not initialized");
        return;
    };

    let cmd = serde_json::json!({
        "type": "call",
        "name": module_name,
        "fn": export_name,
        "args": call_args,
    });
    let rx = match runtime.send_command(cmd) {
        Ok(rx) => rx,
        Err(e) => {
            ipc_callback_err(&callback, 500, &e);
            return;
        }
    };
    crate::state::rt_handle().spawn(async move {
        match rx.await {
            Ok(Ok(response)) => {
                let val = response
                    .get("result")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                ipc_callback_ok(&callback, &val.to_string());
            }
            Ok(Err(e)) => ipc_callback_err(&callback, 500, &e),
            Err(_) => ipc_callback_err(&callback, 500, "Bun response channel dropped"),
        }
    });
}

#[cfg(test)]
#[path = "../../../tests/unit/ipc/plugin/native.rs"]
mod tests;
