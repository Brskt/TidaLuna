use super::plugin_ipc::CleanupScope;
use super::trust_prompt::{Prompts, recordable};
use super::{ipc_callback_err, ipc_callback_ok};
use crate::app_state::{IpcCallback, IpcMessage};
use crate::native_runtime::NativeRuntime;
use crate::plugins::name::PluginName;
use crate::state::{NATIVE_RUNTIME, NATIVE_RUNTIME_INIT};

use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

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
}

impl ModuleEntry {
    fn fresh() -> Self {
        Self {
            epoch: NEXT_ENTRY_EPOCH.fetch_add(1, Ordering::Relaxed),
            in_flight: 0,
            channel: None,
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
fn issue_native_channel(name: &str, code_hash: &str) -> Option<String> {
    let mut modules = NATIVE_MODULES.lock().unwrap_or_else(|e| e.into_inner());
    let entry = modules
        .entry(name.to_string())
        .or_insert_with(ModuleEntry::fresh);
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
        let modules = NATIVE_MODULES.lock().unwrap_or_else(|e| e.into_inner());
        if modules
            .get(&self.module)
            .is_none_or(|entry| entry.epoch != self.epoch)
        {
            return Err("registerNative: the plugin was uninstalled".to_string());
        }
        send()
    }
}

impl Drop for PendingRegistration {
    fn drop(&mut self) {
        let mut modules = NATIVE_MODULES.lock().unwrap_or_else(|e| e.into_inner());
        // An entry that is gone, or is another incarnation's, is not this attempt's to settle: an
        // uninstall cleared it while the attempt was in flight, and a reinstall may have made a new
        // one whose own attempts would otherwise be miscounted here.
        let Some(entry) = modules
            .get_mut(&self.module)
            .filter(|entry| entry.epoch == self.epoch)
        else {
            return;
        };
        entry.in_flight = entry.in_flight.saturating_sub(1);
        // An attempt that never produced a channel leaves nothing behind; the slot goes back.
        if entry.in_flight == 0 && entry.channel.is_none() {
            modules.remove(&self.module);
        }
    }
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
pub(super) fn clear_pending_trust(scope: &CleanupScope) {
    match scope {
        CleanupScope::All => PENDING_TRUST.abandon_where(|_| true),
        CleanupScope::Plugin(owner) => {
            PENDING_TRUST.abandon_where(|key| module_belongs_to(pending_trust_module(key), owner))
        }
    }
}

/// A reinstall's reset, run on the actor: the open prompts first, which shuts the gate of an answer
/// still on its way, then the rows. An answer written before it is cleared; one after it writes
/// nothing.
pub(super) fn reset_trust(
    sc: &mut rusqlite::Connection,
    plugin: &PluginName,
) -> rusqlite::Result<()> {
    clear_pending_trust(&CleanupScope::Plugin(plugin.clone()));
    crate::native_runtime::trust::clear_trust_by_plugin(sc, plugin)
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
/// uninstall has already happened. Without a child (no native plugin ever registered) there is
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
    let front = crate::frontend::load::get().ok_or_else(|| {
        "Failed to start native runtime: the front files were not loaded".to_string()
    })?;
    let rt = NativeRuntime::spawn(crate::state::rt_handle(), &front.native_host)
        .map_err(|e| format!("Failed to start native runtime: {e}"))?;
    crate::vprintln!("[NATIVE] Bun process started");
    if NATIVE_RUNTIME.set(rt).is_err() {
        panic!("NATIVE_RUNTIME already initialized under init lock");
    }
    Ok(NATIVE_RUNTIME.get().unwrap())
}

pub(super) fn handle_register_native(
    msg: &IpcMessage,
    caller: &crate::ipc::caller::Caller,
    callback: IpcCallback,
) {
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

    // The acting plugin is the capability's, never argument 0's. Read off the argument, this only
    // ever confirmed that SOME plugin bore the claimed name. A caller naming another one got that
    // plugin's `data_dir`, inherited its trust grants whenever the code hashed the same, and could
    // hold its eight module slots until it was uninstalled. The dispatcher refuses this channel
    // unattributed; the arm below is the floor under that gate rather than its own gate.
    let plugin_url = match caller {
        crate::ipc::caller::Caller::Plugin(url) => url.clone(),
        crate::ipc::caller::Caller::Unattributed => {
            crate::vprintln!("[NATIVE] Refused registerNative for '{name}': unattributed call");
            ipc_callback_err(&callback, 403, "unattributed call");
            return;
        }
    };

    // The load id is captured here rather than re-derived later, because it must answer "still this
    // load" after the awaits below; it is `Some` exactly when the plugin is loaded. The declared
    // name comes with it, from the same read: two reads could straddle a reload and describe two
    // different loads.
    let live = crate::app_state::with_state(|state| {
        let declared = state.plugin_manager.name_for_url(&plugin_url)?.clone();
        let load_id = state.plugin_manager.current_load_id(&plugin_url)?;
        // Admission is also a step of a load still waiting on its ack, taken under the lock its
        // watchdog looks under: either the step lands first, or the load is already gone.
        let progress = state.plugin_manager.load_progress(&plugin_url, load_id);
        if let Some(progress) = &progress {
            progress.step();
        }
        Some((declared, load_id, progress))
    })
    .flatten();
    let Some((declared_name, load_id, progress)) = live else {
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

    // The argument still carries the MODULE file, which is its own to name; the plugin half of it
    // is a claim, and this is where it stops being taken on trust. Refused loudly rather than
    // rewritten to the declared name: a rewrite would move a plugin whose bundle disagrees with
    // its manifest to a different `data_dir`, silently leaving the data it had written behind.
    if declared_name.as_str() != plugin_prefix {
        crate::verr!(
            "[NATIVE] Refused registerNative for '{name}': the caller declares '{declared_name}', not '{plugin_prefix}'"
        );
        ipc_callback_err(
            &callback,
            403,
            "registerNative: the module names another plugin",
        );
        return;
    }

    // Reserved before Bun is touched; a caller looping fresh module names cannot grow its module
    // table. Held for the whole registration since the ledger only writes once Bun answers;
    // unawaited concurrent calls would otherwise all pass the check first.
    let Some(reservation) = PendingRegistration::reserve(&name, &declared_name) else {
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

    // Spawns the Bun child the first time a session needs one, on this thread. It stays here on
    // purpose: a `OnceLock` makes it once-per-process and it waits on nothing afterwards, while
    // moving the spawn out of a tokio runtime context is what crashed it before.
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

    // One plain folder under the native root, or two for `@scope/name`: the type admits no name
    // that is the root itself or leaves it.
    let data_dir_path = declared_name.data_dir_under(&native_data_dirs());
    let data_dir = data_dir_path.to_string_lossy().to_string();

    // Everything above is in-memory and stays here. The two reads leave: this handler is entered
    // on the CEF UI thread, where each actor round trip froze the window. They travel as one trip
    // because the actor hands out both connections at once, and they keep the keys they already
    // used: the manifest by the declared name, the grants by module name and code hash together.
    crate::state::rt_handle().spawn(async move {
        let read = tokio::task::spawn_blocking({
            let declared = declared_name.clone();
            let plugin = name.clone();
            let hash = code_hash.clone();
            move || {
                crate::state::db().call(move |pc, sc| {
                    // A case twin installed before names were unique in any case shares this
                    // folder wherever the file system ignores case.
                    if crate::plugins::store::case_twin_installed(pc, &declared) {
                        return Err(DataDirRefusal::SharedByCase);
                    }
                    let manifest_json: String = pc
                        .query_row(
                            "SELECT manifest FROM plugins WHERE name = ?1 AND installed = 1",
                            rusqlite::params![declared.as_str()],
                            |row| row.get(0),
                        )
                        .unwrap_or_default();
                    let decisions = crate::native_runtime::trust::load_trust(sc, &plugin, &hash);
                    Ok((manifest_json, decisions))
                })
            }
        })
        .await;

        let (manifest_json, decisions) = match read {
            Ok(Ok(pair)) => pair,
            Ok(Err(DataDirRefusal::SharedByCase)) => {
                crate::verr!(
                    "[NATIVE] Refused the data dir of {:?}: another installed plugin has the same name in another case",
                    declared_name.as_str()
                );
                ipc_callback_err(
                    &callback,
                    403,
                    "registerNative: another installed plugin has this name in another case",
                );
                return;
            }
            Err(e) => {
                // Refusing beats registering with no grants read: an empty map reads as "nothing
                // was ever trusted" and would re-prompt for modules the user already answered.
                crate::verr!("[NATIVE] registerNative state read did not run: {e}");
                ipc_callback_err(
                    &callback,
                    500,
                    "registerNative: could not read plugin state",
                );
                return;
            }
        };
        let trust_grants: HashMap<String, bool> = decisions
            .into_iter()
            .map(|d| (d.module, d.granted))
            .collect();

        let scanned = scanned_rows(runtime, &code).await;
        if let Some(progress) = &progress {
            progress.step();
        }
        let mut attempt = RegisterAttempt {
            code,
            code_hash,
            trust_grants,
            manifest_json,
            data_dir,
            plugin_url,
            load_id,
            progress,
            reservation,
        };
        // One dialog for what the file needs, before it runs, not one per evaluation stopping at
        // the first. Only unanswered rows: a stored refusal is never asked again nor turned into an
        // allow. A denied row does not stop the registration; the gate refuses a module reaching it.
        let rows: Vec<&'static str> = scanned
            .into_iter()
            .filter(|row| {
                row_modules(row)
                    .iter()
                    .any(|module| !attempt.trust_grants.contains_key(*module))
            })
            .collect();
        if !rows.is_empty() && ask_trust(&mut attempt, &rows).await.is_none() {
            prompt_ended_unanswered(&callback, attempt.reservation.module(), &rows.join(", "));
            return;
        }
        do_register(runtime, attempt, callback);
    });
}

/// The rows `code` names in a literal `require`, from a scan Bun runs without
/// evaluating anything. A hint: a scan that fails or misses a name leaves it to the sentinel,
/// one row at a time, and the gate in Bun decides what loads either way.
async fn scanned_rows(runtime: &NativeRuntime, code: &str) -> Vec<&'static str> {
    let Ok(rx) = runtime.send_command(serde_json::json!({ "type": "scan", "code": code })) else {
        return Vec::new();
    };
    let Ok(Ok(response)) = rx.await else {
        return Vec::new();
    };
    let found: Vec<&'static str> = response
        .get("required")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str())
        .filter_map(trust_row)
        .collect();
    TRUST_ROWS
        .iter()
        .map(|(row, _)| *row)
        .filter(|row| found.contains(row))
        .collect()
}

/// Ask the user about `rows`, record the answers, and fold them into the attempt's grants.
/// `None` when the prompt ended unanswered (closed, abandoned, or never shown): nothing was
/// recorded, and the next load asks again.
async fn ask_trust(attempt: &mut RegisterAttempt, rows: &[&'static str]) -> Option<Arc<[bool]>> {
    // Until the caller has dispatched what comes next, the ack watchdog waits on the user.
    let _user = attempt.progress.as_ref().map(|progress| progress.wait());
    // Attempts of one load asking the same rows share a dialog. The dialog answers the prompt it
    // was opened for and no other; a prompt of another load is abandoned and re-opened.
    let entered = PENDING_TRUST.enter(
        attempt.reservation.module(),
        &rows.join(","),
        attempt.load_id,
    );
    let mut waiting = entered.waiting;
    if let Some(answerer) = entered.answerer {
        let dialog_rx = crate::ui::trust_dialog::show_trust_dialog(
            attempt.reservation.module(),
            rows,
            &attempt.manifest_json,
        );
        let count = rows.len();
        // The answer is broadcast without ending the prompt: late attempts of the load still
        // dedup against it. The prompt ends once the decision is written.
        crate::state::rt_handle().spawn(async move {
            match dialog_rx.await {
                Ok(crate::ui::trust_dialog::TrustReply::Answered(answers))
                    if answers.len() == count =>
                {
                    answerer.answer(answers)
                }
                // Closed, a malformed answer, or a window gone away: nobody decided anything.
                _ => answerer.withdraw(),
            }
        });
    }

    let Some(answers) = recordable(waiting.decide().await) else {
        PENDING_TRUST.finish(&waiting);
        crate::vprintln!(
            "[NATIVE] Trust prompt for '{}' -> {} ended unanswered",
            attempt.reservation.module(),
            rows.join(", ")
        );
        return None;
    };

    // Recorded in one closure: a reinstall lands before every row of the answer or after all of them.
    {
        let hash = attempt.code_hash.clone();
        let plugin = attempt.reservation.module().to_string();
        let decided: Vec<(&'static str, bool)> = rows
            .iter()
            .zip(answers.iter())
            .flat_map(|(row, &granted)| row_modules(row).iter().map(move |m| (*m, granted)))
            .collect();
        let gate = waiting.gate();
        // Awaited rather than posted: the dedup key below may only go once this write is
        // committed, or a caller after it would find neither the key nor the row and prompt
        // again. Not synced to the disk: a commit a power cut takes back asks again, since
        // `clear_trust_by_plugin` leaves no older grant to read back. Handed to the blocking
        // pool: the round trip parks that pool's thread instead of this tokio worker.
        let written = tokio::task::spawn_blocking(move || {
            crate::state::db().call_settings(move |conn| {
                // Checked on the actor, where an uninstall or a reinstall also runs: either
                // abandoned the prompt first or clears the rows after. On the waiter's side, a
                // grant read before it would be written after it.
                if !gate.is_open() {
                    return;
                }
                for (module, granted) in decided {
                    if let Err(e) = crate::native_runtime::trust::save_trust(
                        conn, &hash, &plugin, module, granted,
                    ) {
                        crate::vprintln!(
                            "[NATIVE] Failed to save trust for {}::{}: {}",
                            plugin,
                            module,
                            e
                        );
                    }
                }
            })
        })
        .await;
        if let Err(e) = written {
            // The key still goes below. A decision that never reached the disk must re-prompt,
            // which is what losing the key produces; keeping it would dedup a later caller
            // against a grant nothing recorded.
            crate::verr!("[NATIVE] Trust write did not run: {e}");
        }
    }
    PENDING_TRUST.finish(&waiting);
    // Abandoned between the answer and the write: nothing was recorded.
    if !waiting.gate().is_open() {
        return None;
    }

    for (row, &granted) in rows.iter().zip(answers.iter()) {
        for module in row_modules(row) {
            attempt.trust_grants.insert(module.to_string(), granted);
        }
    }
    Some(answers)
}

/// Why a plugin's data dir cannot be handed out.
enum DataDirRefusal {
    /// Another installed plugin's name differs only by case: one folder where case is ignored.
    SharedByCase,
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
    /// What the load's ack watchdog reads, while the load still waits on its ack.
    progress: Option<std::sync::Arc<crate::plugins::manager::LoadProgress>>,
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
    // Carried per registration rather than in the child's environment: the sandbox can
    // withhold a register field from an untrusted plugin, and cannot withhold an env var
    // from anyone. The host decides who receives it; no predicate lives in two places.
    let cmd = serde_json::json!({
        "type": "register",
        "name": attempt.reservation.module(),
        "code": attempt.code,
        "trust": trust_json,
        "dataDir": attempt.data_dir,
        // The host holds the data dir to this root on its own, on the canonical path: a second
        // check behind the name rule, which already keeps every data dir strictly inside it.
        "nativeRoot": native_data_dirs().to_string_lossy(),
        "runtimeDir": std::env::var("XDG_RUNTIME_DIR").ok(),
    });
    // Also reached from the trust retry below, possibly long after an uninstall: the one place a
    // register line is written, and the one place that checks its entry first.
    let rx = match attempt
        .reservation
        .send_if_current(|| runtime.send_command(cmd))
    {
        Ok(rx) => rx,
        Err(e) => {
            ipc_callback_err(&callback, 500, &e);
            return;
        }
    };

    crate::state::rt_handle().spawn(async move {
        let answer = rx.await;
        if let Some(progress) = &attempt.progress {
            progress.step();
        }
        match answer {
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
                    clear_pending_trust_for_module(attempt.reservation.module());
                    ipc_callback_err(&callback, 500, "registerNative: plugin is no longer active");
                    return;
                }
                let Some(token) =
                    issue_native_channel(attempt.reservation.module(), &attempt.code_hash)
                else {
                    clear_pending_trust_for_module(attempt.reservation.module());
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
                clear_pending_trust_for_module(attempt.reservation.module());
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

                    // The gate never throws for a module it was handed: a sentinel for an allowed
                    // one is the plugin's own, and answering it would re-ask in a loop.
                    if attempt.trust_grants.get(&module) == Some(&true) {
                        ipc_callback_err(&callback, 500, &e);
                        return;
                    }
                    // The sentinel only ever names a module the gate holds back; anything else is
                    // no row of ours and is answered as the failure it is.
                    let Some(row) = trust_row(&module) else {
                        ipc_callback_err(&callback, 500, &e);
                        return;
                    };

                    crate::vprintln!(
                        "[NATIVE] Trust required for '{}' -> module '{}'",
                        attempt.reservation.module(),
                        module
                    );

                    // A name the scan missed: the same dialog, with its one row.
                    let Some(answers) = ask_trust(&mut attempt, &[row]).await else {
                        prompt_ended_unanswered(&callback, attempt.reservation.module(), &module);
                        return;
                    };

                    // Denied, and this time Bun did reach it while loading: nothing to retry.
                    if !answers[0] {
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
