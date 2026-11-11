use super::trust_prompt::Prompts;
use super::uninstall::CleanupScope;
use super::{ipc_callback_err, ipc_callback_ok};
use crate::app_state::{IpcCallback, IpcMessage};
use crate::plugins::name::PluginName;
use crate::state::NATIVE_RUNTIME;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};

mod register;

pub(super) use register::handle_register_native;

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

/// The rows a trust dialog can show, each with the modules its answer covers, in display order. A
/// closed list: Bun's scan and its sentinel are both read through it, which keeps any name a
/// plugin wrote out of a dialog, a key and the table.
const TRUST_ROWS: &[(&str, &[&str])] = &[
    ("net", NETWORK_MODULES),
    ("fs", &["fs"]),
    ("os", &["os"]),
    ("dgram", &["dgram"]),
    ("diagnostics_channel", &["diagnostics_channel"]),
    ("worker_threads", &["worker_threads"]),
];

/// The row that asks for `dependency`, `fs/promises` answering with `fs` as Bun's gate does.
fn trust_row(dependency: &str) -> Option<&'static str> {
    let dependency = if dependency == "fs/promises" {
        "fs"
    } else {
        dependency
    };
    TRUST_ROWS
        .iter()
        .find(|(_, modules)| modules.contains(&dependency))
        .map(|(row, _)| *row)
}

/// The modules one answer to `row` decides.
fn row_modules(row: &str) -> &'static [&'static str] {
    TRUST_ROWS
        .iter()
        .find(|(name, _)| *name == row)
        .map_or(&[], |(_, modules)| *modules)
}

/// One refused module refuses its whole row, whatever its other modules stored: Bun honours grants
/// module by module, and a row an older build saved module by module can hold a refusal beside
/// grants. A row refused here is never asked again, so its answer cannot overwrite the refusal.
fn refuse_whole_rows(grants: &mut HashMap<String, bool>) {
    for (_, modules) in TRUST_ROWS {
        if modules.iter().any(|m| grants.get(*m) == Some(&false)) {
            for module in *modules {
                grants.insert((*module).to_string(), false);
            }
        }
    }
}

/// Pending trust prompts: one per "module::rows" and load of the plugin. Concurrent register calls
/// of one load that ask the same rows share a single dialog.
static PENDING_TRUST: LazyLock<Prompts> = LazyLock::new(Prompts::default);

/// Every native module this process holds or is acquiring, keyed by module name.
///
/// One ledger under one lock: splitting settled and in-flight state let a registration settle between
/// the two reads and pass the bound, let concurrent attempts on one module retire each other, and
/// double-counted a re-registration into a spurious 403. Keying by module makes counting a scan of
/// distinct entries: none of the three is expressible.
static NATIVE_MODULES: LazyLock<Mutex<HashMap<String, ModuleEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Hands each ledger entry an identity of its own. The entry is keyed by module name, and a name
/// outlives an uninstall: a reinstall of the same plugin makes an entry under the same key, which an
/// attempt admitted before the uninstall must not mistake for its own.
static NEXT_ENTRY_EPOCH: AtomicU64 = AtomicU64::new(1);

struct ModuleEntry {
    /// Which incarnation of the entry this is. Never reused, unlike the module name.
    epoch: u64,
    /// Attempts in flight for this module. The entry outlives every one of them; no attempt can
    /// retire a module another is still acquiring.
    in_flight: usize,
    /// The channel once Bun has answered, `None` while the module is only being acquired.
    channel: Option<Channel>,
    /// Bun was sent this module's code. Leaving the ledger with it set takes the module down in Bun
    /// too: nothing else would reach or unload it there.
    loaded: bool,
}

impl ModuleEntry {
    fn fresh() -> Self {
        Self {
            epoch: NEXT_ENTRY_EPOCH.fetch_add(1, Ordering::Relaxed),
            in_flight: 0,
            channel: None,
            loaded: false,
        }
    }
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
fn channel_token(entry: &mut ModuleEntry, code_hash: &str) -> Option<String> {
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
struct PendingRegistration {
    module: String,
    /// The ledger entry this attempt reserved. A reinstall's entry under the same name is
    /// another one.
    epoch: u64,
}

impl PendingRegistration {
    /// Claim `name` for this attempt, or `None` if the plugin is at the bound.
    ///
    /// A module the plugin already holds or is already acquiring needs no new slot; anything else
    /// counts against the bound. One lock for the whole decision leaves nothing to settle underneath.
    fn reserve(name: &str, owner: &PluginName) -> Option<Self> {
        let mut modules = NATIVE_MODULES.lock().unwrap_or_else(|e| e.into_inner());
        if !modules.contains_key(name)
            && !admits_new_module(modules.keys().map(String::as_str), owner)
        {
            return None;
        }
        let entry = modules
            .entry(name.to_string())
            .or_insert_with(ModuleEntry::fresh);
        entry.in_flight += 1;
        Some(Self {
            module: name.to_string(),
            epoch: entry.epoch,
        })
    }

    /// The module this attempt is for. The attempt reads its name from here rather than keeping a
    /// second copy: one owner, and nothing can drift between them.
    fn module(&self) -> &str {
        &self.module
    }

    /// Run `send` under the ledger's lock, only while this attempt's entry is still the ledger's.
    /// An uninstall removes the entry and queues its unload under that lock: a send lands ahead of
    /// the unload or is refused, never after it to load a module just taken down.
    fn send_if_current<T>(&self, send: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
        let mut modules = NATIVE_MODULES.lock().unwrap_or_else(|e| e.into_inner());
        let Some(entry) = modules
            .get_mut(&self.module)
            .filter(|entry| entry.epoch == self.epoch)
        else {
            return Err("registerNative: the plugin was uninstalled".to_string());
        };
        let sent = send()?;
        entry.loaded = true;
        Ok(sent)
    }

    /// The channel token of this attempt's module, minted only while the entry is still the one it
    /// reserved: an uninstall or a reinstall between the check of the load and here took it, and a
    /// token put back would reach a module Bun already unloaded, or the reinstall's own.
    fn mint(&self, code_hash: &str) -> Result<String, String> {
        let mut modules = NATIVE_MODULES.lock().unwrap_or_else(|e| e.into_inner());
        let entry = modules
            .get_mut(&self.module)
            .filter(|entry| entry.epoch == self.epoch)
            .ok_or_else(|| "registerNative: the plugin was uninstalled".to_string())?;
        channel_token(entry, code_hash).ok_or_else(|| "RNG unavailable".to_string())
    }
}

impl Drop for PendingRegistration {
    fn drop(&mut self) {
        let mut modules = NATIVE_MODULES.lock().unwrap_or_else(|e| e.into_inner());
        // Under the lock, as a clear unloads: a later attempt's register lands behind this unload.
        if release(&mut modules, &self.module, self.epoch) {
            unload_in_child(&self.module);
        }
    }
}

/// Settle one attempt on `module`. An attempt that never produced a channel leaves nothing behind:
/// the slot goes back with the last one. True when the entry left with Bun holding its code; the
/// caller unloads it there. Pure over the map it is given.
fn release(modules: &mut HashMap<String, ModuleEntry>, module: &str, epoch: u64) -> bool {
    // An entry that is gone, or is another incarnation's, is not this attempt's to settle: an
    // uninstall cleared it while the attempt was in flight, and a reinstall may have made a new
    // one whose own attempts would otherwise be miscounted here.
    let Some(entry) = modules.get_mut(module).filter(|entry| entry.epoch == epoch) else {
        return false;
    };
    entry.in_flight = entry.in_flight.saturating_sub(1);
    if entry.in_flight > 0 || entry.channel.is_some() {
        return false;
    }
    modules.remove(module).is_some_and(|entry| entry.loaded)
}

/// Has this plugin room for one more distinct module? Counts the ledger's own entries, unique by
/// construction: no module is counted twice however many attempts it has in flight. Refuses rather
/// than evicting the oldest: an evicted token unreaches a module but does not unload it from Bun;
/// eviction would bound this ledger while the child kept growing. Pure, and testable off the
/// process-global the other tests share.
fn admits_new_module<'a>(held: impl Iterator<Item = &'a str>, owner: &PluginName) -> bool {
    held.filter(|module| module_belongs_to(module, owner))
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

/// The module a pending-trust key is for. Keys are "module::rows", and rows come from
/// `TRUST_ROWS`, which hold no "::".
fn pending_trust_module(key: &str) -> &str {
    key.rsplit_once("::").map_or(key, |(module, _)| module)
}

/// Abandon the pending prompts of an uninstall or a reinstall. The waiters learn it was the system,
/// not the user.
fn clear_pending_trust(scope: &CleanupScope) {
    match scope {
        CleanupScope::All => PENDING_TRUST.abandon_where(|_| true),
        CleanupScope::Plugin(owner) => {
            PENDING_TRUST.abandon_where(|key| module_belongs_to(pending_trust_module(key), owner))
        }
    }
}

/// An install's, a reinstall's or an uninstall's reset, run on the actor: the open prompts first,
/// which shuts the gate of an answer still on its way, then the rows. An answer written before it
/// is cleared; one after it writes nothing.
pub(super) fn reset_trust(
    sc: &mut rusqlite::Connection,
    plugin: &PluginName,
) -> rusqlite::Result<()> {
    clear_pending_trust(&CleanupScope::Plugin(plugin.clone()));
    crate::native_runtime::trust::clear_trust_by_plugin(sc, plugin)
}

/// An uninstall's reset, run on the actor once the loads are gone: the open prompts first, then the
/// rows, as `reset_trust` does. A load still running could ask again after an earlier clear.
pub(super) fn forget_trust(
    sc: &mut rusqlite::Connection,
    scope: &CleanupScope,
) -> rusqlite::Result<()> {
    match scope {
        CleanupScope::All => {
            clear_pending_trust(scope);
            crate::native_runtime::trust::clear_all_trust(sc)
        }
        CleanupScope::Plugin(plugin) => reset_trust(sc, plugin),
    }
}

/// Abandon the pending prompts of one module, once its registration has settled.
fn clear_pending_trust_for_module(module: &str) {
    PENDING_TRUST.abandon_where(|key| pending_trust_module(key) == module);
}

/// The answer to an attempt whose trust prompt the system dropped. Nobody decided; nothing was
/// recorded and the next attempt asks again.
fn prompt_ended_unanswered(callback: &IpcCallback, module: &str, dependency: &str) {
    ipc_callback_err(
        callback,
        403,
        &format!("Access to module '{dependency}' for '{module}' ended without an answer"),
    );
}

/// Drop a plugin's channel tokens on uninstall: a leftover renderer closure would keep invoking
/// exports whose grants were just withdrawn. Not on disable, where the cleanup handler still runs.
/// Also unloads each module from Bun, where it would live on; the ordering is in `send_if_current`.
pub(super) fn clear_native_channels(scope: &CleanupScope) {
    clear_native_channels_with(scope, unload_in_child);
}

fn clear_native_channels_with(scope: &CleanupScope, mut unload: impl FnMut(&str)) {
    let mut modules = NATIVE_MODULES.lock().unwrap_or_else(|e| e.into_inner());
    for module in remove_scope(&mut modules, scope) {
        unload(&module);
    }
}

/// Take the scope's entries out of the ledger, in-flight ones included, and name them. Pure over
/// the map it is given.
fn remove_scope(modules: &mut HashMap<String, ModuleEntry>, scope: &CleanupScope) -> Vec<String> {
    match scope {
        CleanupScope::All => modules.drain().map(|(module, _)| module).collect(),
        CleanupScope::Plugin(owner) => {
            let gone: Vec<String> = modules
                .keys()
                .filter(|module| module_belongs_to(module, owner))
                .cloned()
                .collect();
            for module in &gone {
                modules.remove(module);
            }
            gone
        }
    }
}

/// Ask the child to unload one module and forget its cookies. Nothing awaits the answer: the
/// entry has left the ledger already. Without a child (no native plugin ever registered) there is
/// nothing to unload.
fn unload_in_child(module: &str) {
    crate::native_runtime::forget_module_cookies(module);
    let Some(runtime) = NATIVE_RUNTIME.get() else {
        return;
    };
    if let Err(e) = runtime.send_command(serde_json::json!({ "type": "cleanup", "name": module })) {
        crate::verr!("[NATIVE] Could not unload '{module}' from Bun: {e}");
    }
}

/// A plugin owns its bare-named entry module and every "{plugin}/{file}" one. No owner reaches
/// every module: an uninstall of everything is `CleanupScope::All`, never a name.
fn module_belongs_to(module: &str, owner: &PluginName) -> bool {
    // The separator stops "foo" matching "foobar". Split rather than formatted: this runs once per
    // held module on every registration and every uninstall.
    module
        .strip_prefix(owner.as_str())
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// The folder holding one data dir per native plugin, which the install guard leaves writable.
pub(crate) fn native_data_dirs() -> std::path::PathBuf {
    crate::state::cache_data_dir().join("native")
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
#[path = "../../../../tests/unit/ipc/plugin/native.rs"]
mod tests;
