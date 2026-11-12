use cef::*;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

pub(crate) type IpcCallback = Arc<Mutex<dyn cef::wrapper::message_router::BrowserSideCallback>>;

#[derive(Deserialize, Debug)]
pub(crate) struct IpcMessage {
    pub(crate) channel: String,
    #[serde(default)]
    pub(crate) args: Vec<serde_json::Value>,
    #[serde(default)]
    pub(crate) id: Option<String>,
    /// Caller capability, resolved to a `Caller` at ingress. A reserved envelope field, not a
    /// positional argument: `arg(i)` indices stay correct; absent rather than a parse failure for
    /// unattributed callers. Omitted from `Display` below on purpose. It must not reach the log.
    #[serde(default)]
    pub(crate) cap: Option<String>,
}

impl std::fmt::Display for IpcMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "IpcMessage {{ channel: {:?}, args: [", self.channel)?;
        for (i, arg) in self.args.iter().enumerate() {
            if i > 0 {
                write!(f, ", ")?;
            }
            let s = arg.to_string();
            if s.len() > 200 {
                write!(
                    f,
                    "{}...({} chars)",
                    crate::util::truncate_str(&s, 200),
                    s.len()
                )?;
            } else {
                write!(f, "{s}")?;
            }
        }
        write!(f, "]")?;
        if let Some(id) = &self.id {
            write!(f, ", id: {id:?}")?;
        }
        write!(f, " }}")
    }
}

impl IpcMessage {
    pub(crate) fn arg(&self, index: usize) -> &str {
        self.args.get(index).and_then(|v| v.as_str()).unwrap_or("")
    }
}

/// A track length and the track it was measured on. The identity cannot be recovered
/// later: the current-metadata slot has already moved on to the next track.
pub(crate) struct MeasuredDuration {
    /// `None` when the payload named no track, and such a measurement matches nothing: a
    /// length is never lent to another track.
    pub(crate) track_id: Option<String>,
    pub(crate) secs: f64,
}

impl MeasuredDuration {
    /// Mints a measurement, holding the field above to what it claims: a blank id names no
    /// track. Every producer goes through here, and an ingress that skips its own normalizing
    /// still cannot mint an id equal to the next blank one.
    pub(crate) fn new(track_id: Option<String>, secs: f64) -> Self {
        Self {
            track_id: track_id
                .as_deref()
                .and_then(crate::util::metadata::trimmed_non_empty),
            secs,
        }
    }
}

pub(crate) struct AppState {
    pub(crate) player: Arc<crate::player::Player>,
    pub(crate) pending_time_update: Option<(f64, u32)>,
    pub(crate) pending_player_events: Vec<crate::bridge::PlayerBridgeEvent>,
    pub(crate) pending_misc_js: Vec<String>,
    pub(crate) browser: Option<Browser>,
    pub(crate) flush_scheduled: bool,
    pub(crate) media_controls: Option<crate::platform::media_controls::OsMediaControls>,
    pub(crate) media_duration: Option<MeasuredDuration>,
    pub(crate) plugin_manager: crate::plugins::PluginManager,
    pub(crate) captured_token: String,
    pub(crate) token_state: Option<crate::platform::secure_store::StoredTokenState>,
    /// Keyed by CEF's own per-query id, never by the `id` the caller sent. Three independent
    /// JS counters feed this map (`fetch.__seq` per plugin load, `@luna/lib`'s bundle-wide
    /// one, the early-runtime's page-global one) and all start at 1; a client-chosen key
    /// collides across plugins, across reloads of one plugin, and across channels.
    pub(crate) pending_ipc_callbacks: HashMap<i64, IpcCallback>,
    pub(crate) pending_window_save: Option<crate::settings::WindowState>,
    pub(crate) window_save_scheduled: bool,
    #[cfg(target_os = "windows")]
    pub(crate) thumbbar: Option<crate::platform::thumbbar::ThumbBar>,
    pub(crate) close_to_tray: bool,
    pub(crate) force_quit: bool,
    pub(crate) needs_proactive_refresh: bool,
    pub(crate) needs_blob_purge: bool,
    // Gate: plugin JS must not load until the cold-boot real OAuth token has been
    // rotated to opaque nonces in TIDAL's SDK. Default true (warm boot / no
    // session); a cold boot with a pending refresh closes it.
    pub(crate) proactive_refresh_done: bool,
    // Plugin-load requests parked behind the gate or behind a running pass, each tagged with the
    // session and page it was made under; answered when a pass of that same ticket settles.
    pub(crate) plugin_load_waiters: Vec<(Ticket, IpcCallback)>,
    // Set to the ticket of the load pass currently running off the UI thread. One at a time: two
    // concurrent passes inject every enabled plugin twice into the same frame, and only one of
    // the two can ever have its ready ack accepted.
    pub(crate) plugin_load_in_flight: Option<Ticket>,
    // Bumped on every session change, and on nothing else. Work started under an older epoch
    // describes a session the user has left: a plugin-load pass must answer nobody who asked
    // after the change, and a token minted before it must not be committed at all (in memory
    // or to disk) because `captured_token` is the live authorization, not a draft.
    pub(crate) session_epoch: u64,
    // The main document now running, and what this page's plugins have done in it.
    pub(crate) page: PagePhase,
    // Set by the first plugin admitted in this process and never cleared: from then on any
    // script of the page may be a plugin's, and `plugin.migrate_base_url` refuses them all.
    pub(crate) plugin_code_admitted: bool,
    pub(crate) connect: Option<crate::connect::ConnectManager>,
}

/// What the slot becomes when a measurement arrives, stated without an `AppState` behind it.
///
/// An untagged measurement names no track: it can never satisfy `same_track`. Storing one
/// buys nothing and costs whichever tagged length a frame had yet to claim.
fn settle_recorded_duration(
    current: Option<MeasuredDuration>,
    measured: MeasuredDuration,
) -> Option<MeasuredDuration> {
    if measured.track_id.is_none() {
        return current;
    }
    Some(measured)
}

/// The session and the main document a piece of plugin work was started for. Neither outlives a
/// change: a logout bumps the epoch, a reload or a navigation the page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Ticket {
    pub(crate) epoch: u64,
    pub(crate) page: u64,
}

/// What a main document holds of the plugin loading, gone with it. A reload keeps the session but
/// not one plugin of the page: work started for the old document must neither run in the new one,
/// before it pinned the host modules its plugins resolve through, nor be counted there.
#[derive(Default)]
pub(crate) struct PagePhase {
    generation: u64,
    /// How the last load pass of this page ended. The last one wins: a later pass that could not
    /// load everything takes back what an earlier one vouched for.
    pass_end: PassEnd,
    /// The plugins a pass left waiting on a store move's answer, url to name.
    waiting: HashMap<String, String>,
}

#[derive(Default)]
enum PassEnd {
    #[default]
    Pending,
    Completed(Ticket),
    Incomplete(Ticket),
}

/// What a page's load passes stand for, asked under one ticket.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PassStood {
    /// No pass of this ticket ended yet: the pass to come reads the rows itself.
    NotYet,
    /// Every enabled plugin was tried: the ones not failed are live or parked on a store move.
    Completed,
    /// A pass ended without trying every plugin, or was refused: nothing loads the rest here.
    Incomplete,
}

impl PagePhase {
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    /// A new main document: nothing of the old one carries over.
    pub(crate) fn next(&mut self) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        self.pass_end = PassEnd::Pending;
        self.waiting.clear();
        self.generation
    }

    pub(crate) fn mark_loaded(&mut self, ticket: Ticket) {
        self.pass_end = PassEnd::Completed(ticket);
    }

    pub(crate) fn mark_incomplete(&mut self, ticket: Ticket) {
        self.pass_end = PassEnd::Incomplete(ticket);
    }

    pub(crate) fn pass_stood(&self, ticket: Ticket) -> PassStood {
        match self.pass_end {
            PassEnd::Completed(of) if of == ticket => PassStood::Completed,
            PassEnd::Incomplete(of) if of == ticket => PassStood::Incomplete,
            _ => PassStood::NotYet,
        }
    }

    pub(crate) fn wait(&mut self, plugins: HashMap<String, String>) {
        self.waiting.extend(plugins);
    }

    pub(crate) fn take_waiting(&mut self) -> HashMap<String, String> {
        std::mem::take(&mut self.waiting)
    }
}

/// What a session that has just ended was still holding, for the caller to settle outside
/// the lock: an `eval_js` reaches the renderer and a parked callback answers an IPC
/// request, and neither belongs under the state mutex.
#[derive(Default)]
pub(crate) struct EndedSession {
    /// The plugins the ended session had live in the page.
    pub(crate) loaded: Vec<(String, u64)>,
    /// The load requests it parked, to be answered rather than left hanging across the
    /// logout/login transition.
    pub(crate) waiters: Vec<(Ticket, IpcCallback)>,
}

/// One token exchange or refresh, for [`AppState::commit_token_generation`].
pub(crate) struct TokenGrant<'a> {
    pub(crate) access_token: &'a str,
    pub(crate) refresh_token: Option<&'a str>,
    pub(crate) opaque_at: &'a str,
    /// The nonce minted for `refresh_token`, present exactly when it is.
    pub(crate) opaque_rt: Option<&'a str>,
    pub(crate) expires_in: Option<u64>,
    pub(crate) user_id: Option<&'a str>,
    pub(crate) granted_scopes: &'a [String],
    /// The client_id the exchange was sent with, when one was observed.
    pub(crate) client_id: Option<&'a str>,
}

impl TokenGrant<'_> {
    /// A refresh token is bound to the client_id it was minted under (RFC 6749 §6): a NEW one
    /// takes this exchange's. An answer without one keeps `prior`, the replaced generation's, if
    /// set: the app-level `client_credentials` answer must not overwrite the user's client_id.
    pub(crate) fn bound_client_id(&self, prior: Option<&str>) -> String {
        let exchange = self.client_id.filter(|s| !s.is_empty());
        let prior = prior.filter(|s| !s.is_empty());
        if self.refresh_token.is_some() {
            exchange.or(prior)
        } else {
            prior.or(exchange)
        }
        .unwrap_or("")
        .to_string()
    }
}

/// How a token commit ended, for each caller to answer and log in its own terms.
pub(crate) enum TokenCommit {
    /// The session the exchange was issued under has ended: nothing was written.
    Refused,
    /// The grant is the current generation; `durable` says whether it belongs on disk.
    Committed { opaque_rt: String, durable: bool },
}

impl AppState {
    pub(crate) fn new(player: Arc<crate::player::Player>) -> Self {
        AppState {
            player,
            pending_time_update: None,
            pending_player_events: Vec::new(),
            pending_misc_js: Vec::new(),
            browser: None,
            flush_scheduled: false,
            media_controls: None,
            media_duration: None,
            plugin_manager: crate::plugins::PluginManager::new(),
            captured_token: String::new(),
            token_state: None,
            pending_ipc_callbacks: HashMap::new(),
            pending_window_save: None,
            window_save_scheduled: false,
            #[cfg(target_os = "windows")]
            thumbbar: None,
            close_to_tray: false,
            force_quit: false,
            needs_proactive_refresh: false,
            needs_blob_purge: false,
            // Gate open by default; cold boot closes it when a refresh is due.
            proactive_refresh_done: true,
            plugin_load_waiters: Vec::new(),
            plugin_load_in_flight: None,
            session_epoch: 0,
            page: Default::default(),
            plugin_code_admitted: false,
            connect: Some(crate::connect::ConnectManager::new()),
        }
    }

    /// Makes `grant` the current token generation and keeps the one it replaces for 30 seconds.
    pub(crate) fn apply_token_generation(
        &mut self,
        session_epoch: u64,
        grant: &TokenGrant,
    ) -> TokenCommit {
        // The session that issued the exchange may have ended while it was on the wire.
        // Committing then would re-authenticate the live process as the departed account. The
        // bump and this read are critical sections of the one `AppState` mutex, which orders them.
        if self.session_epoch != session_epoch {
            return TokenCommit::Refused;
        }
        let now_secs = crate::util::now_unix_secs();

        let (real_rt, ort) = if let Some(rt) = grant.refresh_token {
            (
                rt.to_string(),
                grant.opaque_rt.unwrap_or_default().to_string(),
            )
        } else if let Some(ref ts) = self.token_state {
            (
                ts.current.refresh_token.clone(),
                ts.current.opaque_rt.clone(),
            )
        } else {
            (String::new(), String::new())
        };

        let client_id = grant.bound_client_id(
            self.token_state
                .as_ref()
                .map(|ts| ts.current.client_id.as_str()),
        );
        let new_gen = crate::platform::secure_store::TokenGeneration {
            access_token: grant.access_token.to_string(),
            refresh_token: real_rt,
            opaque_at: grant.opaque_at.to_string(),
            opaque_rt: ort.clone(),
            version: self
                .token_state
                .as_ref()
                .map(|ts| ts.current.version + 1)
                .unwrap_or(1),
            access_expires: now_secs + grant.expires_in.unwrap_or(3600),
            user_id: grant.user_id.map(str::to_owned),
            granted_scopes: grant.granted_scopes.to_vec(),
            client_id,
        };
        let durable = new_gen.is_durable();

        let previous = self.token_state.as_ref().map(|ts| ts.current.clone());
        self.token_state = Some(crate::platform::secure_store::StoredTokenState {
            current: new_gen,
            previous,
            previous_valid_until: Some(now_secs + 30),
        });

        self.captured_token = grant.access_token.to_string();
        TokenCommit::Committed {
            opaque_rt: ort,
            durable,
        }
    }

    /// [`Self::apply_token_generation`], then the new generation queued to disk when durable.
    pub(crate) fn commit_token_generation(
        &mut self,
        session_epoch: u64,
        grant: &TokenGrant,
    ) -> TokenCommit {
        let commit = self.apply_token_generation(session_epoch, grant);
        if let TokenCommit::Committed { durable, .. } = &commit {
            let data_dir = crate::state::cache_data_dir();
            // Only a durable generation belongs on disk. One without a refresh token would
            // replace a credential that still works, and the next launch would seed the SDK's
            // own blob from it, handing TIDAL an empty refresh token verbatim.
            if let (true, Some(ts)) = (*durable, &self.token_state) {
                crate::platform::secure_store::save_queued(&data_dir, ts);
            }
        }
        commit
    }

    /// The one way `media_duration` is written, and it never empties: a payload that carries
    /// no length is not evidence that the last measured one was wrong.
    pub(crate) fn record_measured_duration(&mut self, measured: MeasuredDuration) {
        self.media_duration = settle_recorded_duration(self.media_duration.take(), measured);
    }

    /// End the session now in progress, and name everything it was still holding.
    ///
    /// One critical section, because the pieces only hold together taken at once. A load pass
    /// runs off the UI thread: retiring the plugins first and bumping after leaves a window
    /// where a pass registers too late for the sweep and reads the epoch too early to be
    /// stopped by it, so the injection lands in a session the user has left with nothing naming
    /// it. Clearing the token later leaves the matching window the other way, where
    /// `handle_jsrt_load_plugins` still sees a credential and starts a pass under the NEW epoch.
    pub(crate) fn end_session(&mut self) -> EndedSession {
        // Named and dropped in one call, rather than as each cleanup is dispatched: what
        // makes the bump below safe is that no entry survives it, and a retire interleaved
        // with an `eval_js` would hand the lock back between the two.
        let loaded = self.plugin_manager.retire_all_loaded();
        // A pass still running belongs to the session being cleared. Bumping stops it at its
        // next plugin and keeps its result from ever answering a request made from here on,
        // which would report the departed session's plugin set as the new one's.
        self.session_epoch = self.session_epoch.wrapping_add(1);
        // Left waiting by this session's pass: the next session's pass loads them itself.
        self.page.take_waiting();
        self.captured_token.clear();
        self.token_state = None;
        EndedSession {
            loaded,
            waiters: std::mem::take(&mut self.plugin_load_waiters),
        }
    }

    /// The session and page plugin work started now belongs to.
    pub(crate) fn ticket(&self) -> Ticket {
        Ticket {
            epoch: self.session_epoch,
            page: self.page.generation(),
        }
    }

    /// End the plugin loading of the page that is gone, returning the new page's generation. Its
    /// plugins died with the document, no unload owed; its parked requests go, or a pass for them
    /// would admit plugins before the new page's bundle asked. A running pass stops by itself.
    pub(crate) fn end_page_plugins(&mut self) -> u64 {
        self.plugin_manager.retire_all_loaded();
        self.plugin_load_waiters.clear();
        self.page.next()
    }

    /// End the music of the page that is gone, and the events it had queued for itself.
    pub(crate) fn end_page_music(&mut self) {
        end_page_music(
            &self.player,
            &mut self.pending_player_events,
            &mut self.pending_time_update,
        );
    }
}

/// What the end of a page owes the music it was playing, stated without an `AppState` behind it.
///
/// Rust plays on its own thread; a page that dies does not take its music with it, and the
/// page that follows boots with nothing loaded: it could neither show the track nor stop it.
/// This host's own music only: a cast plays whether or not a page is loaded here. What was
/// queued describes the dead page's track, and a successor must not be told of it.
fn end_page_music(
    player: &crate::player::Player,
    pending_events: &mut Vec<crate::bridge::PlayerBridgeEvent>,
    pending_time: &mut Option<(f64, u32)>,
) {
    let _ = player.stop(crate::player::StopScope::OwnMusic);
    pending_events.clear();
    *pending_time = None;
}

// SAFETY: `thumbbar` (Windows only) holds raw `ITaskbarList3` pointers and a
// `Cell`, and it is the sole field that is not already `Send`. Sound because the
// taskbar object is created and used solely on the CEF UI thread. Gated to
// Windows on purpose: elsewhere the auto-impl carries `AppState`, and a field that
// stops being `Send` fails the build rather than being absorbed here.
//
// Nothing else here constrains threading: `browser` is a `RefGuard`, souvlaki's
// `MediaControls` wraps agile WinRT objects or a channel and a `JoinHandle`, and
// `BrowserSideCallback` is declared `Send + Sync`. Work that only calls `eval_js` or answers
// an IPC callback may therefore run off the UI thread, `Frame::execute_java_script` reposting
// itself onto it.
#[cfg(target_os = "windows")]
unsafe impl Send for AppState {}

pub(crate) static APP_STATE: std::sync::OnceLock<Arc<Mutex<AppState>>> = std::sync::OnceLock::new();

/// Whether CEF's UI thread exists, letting a task posted to it still be taken.
///
/// Posting earlier is safe on its own. CEF finds no task runner and answers
/// false. It also logs a warning no embedder can configure, and CEF's own
/// internals skip that path for exactly that reason; the two callers an OS
/// event can reach before CEF is up ask here first. Every other `post_task` in
/// the tree sits inside a callback that already proves CEF runs.
static CONTEXT_READY: AtomicBool = AtomicBool::new(false);

/// Called once, from `on_context_initialized`.
pub(crate) fn mark_context_ready() {
    CONTEXT_READY.store(true, Ordering::Release);
}

pub(crate) fn context_ready() -> bool {
    CONTEXT_READY.load(Ordering::Acquire)
}

/// Ends the message loop, turning the single-instance channel away first: a link
/// accepted after the loop ends has no one left to show it. The page ends before the loop closes
/// the remaining windows: plugin work still running finds its ticket gone, and writes nothing.
pub(crate) fn quit_loop() {
    with_state(|s| {
        s.end_page_music();
        s.end_page_plugins();
    });
    crate::platform::app_lock::close_channel();
    quit_message_loop();
}

pub(crate) fn with_state<F, R>(f: F) -> Option<R>
where
    F: FnOnce(&mut AppState) -> R,
{
    APP_STATE.get().map(|s| {
        // Recover on poison instead of panicking: this lock is taken inside CEF
        // `extern "C"` callbacks, where a panic would be UB across the FFI boundary.
        let mut guard = s.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut guard)
    })
}

pub(crate) fn exec_js_on_frame(frame: &Frame, js: &str) {
    let code = CefString::from(js);
    let url = CefString::from("");
    frame.execute_java_script(Some(&code), Some(&url), 0);
}

/// Dispatch JS to the renderer's main frame; false if no frame. Targeting the main
/// frame regardless of origin is safe: payloads are injection-safe and secret-free,
/// and subframe-drivable channels are gated upstream.
pub(crate) fn eval_js(js: &str) -> bool {
    let browser = with_state(|state| state.browser.clone());
    if let Some(Some(browser)) = browser
        && let Some(frame) = browser.main_frame()
    {
        exec_js_on_frame(&frame, js);
        true
    } else {
        false
    }
}

const IPC_EMIT_PREFIX: &str =
    "if(typeof window.__LUNAR_IPC_EMIT__==='function')window.__LUNAR_IPC_EMIT__(";

// Escape U+2028/U+2029 (JS line terminators serde leaves raw inside string
// literals): an embedded value can't terminate the statement.
pub(crate) fn escape_js_line_terminators(s: String) -> String {
    // Fast path: U+2028/U+2029 are vanishingly rare; avoid two full-string
    // scans + an allocation when neither is present (the normal case).
    if !s.contains(['\u{2028}', '\u{2029}']) {
        return s;
    }
    s.replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}

// JSON-encode as a JS string literal: a controlled value can't break out and inject.
pub(crate) fn js_string_literal(s: &str) -> String {
    escape_js_line_terminators(serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string()))
}

/// Build a `__TIDAL_IPC_RESPONSE__(id, null, result)` reply with the
/// renderer-controlled `id` JSON-encoded; it can't break out and inject.
/// `result_json` is already-serialized JSON in value position.
pub(crate) fn js_ipc_response(id: &str, result_json: &str) -> String {
    format!(
        "window.__TIDAL_IPC_RESPONSE__({}, null, {})",
        js_string_literal(id),
        result_json
    )
}

pub(crate) fn emit_ipc_event(channel: &str) {
    let js = format!("{IPC_EMIT_PREFIX}{});", js_string_literal(channel));
    let _ = eval_js(&js);
}

pub(crate) fn emit_ipc_event_with_args(channel: &str, args: &[&str]) {
    let args_js: Vec<String> = args.iter().copied().map(js_string_literal).collect();
    let js = format!(
        "{IPC_EMIT_PREFIX}{},{});",
        js_string_literal(channel),
        args_js.join(",")
    );
    let _ = eval_js(&js);
}

pub(crate) fn emit_ipc_event_with_data(channel: &str, data: &impl serde::Serialize) {
    let json = match serde_json::to_string(data) {
        Ok(j) => escape_js_line_terminators(j),
        Err(_) => return,
    };
    let js = format!("{IPC_EMIT_PREFIX}{},{json});", js_string_literal(channel));
    let _ = eval_js(&js);
}

/// Only allow `https://` URLs to be opened by the OS.
/// Prevents plugins from opening local files, executables, or dangerous protocol handlers.
pub(crate) fn is_safe_open_url(target: &str) -> bool {
    url::Url::parse(target)
        .map(|u| u.scheme() == "https")
        .unwrap_or(false)
}

pub(crate) fn open_in_os(target: impl AsRef<std::ffi::OsStr>) {
    let target = target.as_ref();
    #[cfg(target_os = "windows")]
    {
        // ShellExecuteW, not `cmd /C start`: the latter treats `&` in a URL query
        // string as a command separator, truncating links like `?a=1&token=2`.
        use std::os::windows::ffi::OsStrExt;
        let file: Vec<u16> = target.encode_wide().chain(std::iter::once(0)).collect();
        // On its own thread, unlike the `spawn` calls below. Those return once a child is
        // launched; this one resolves a file association and can hand the work to a COM shell
        // extension, which is unbounded. Most callers here are UI-thread menu items and link
        // clicks, and the window must not stop repainting while the shell thinks.
        std::thread::Builder::new()
            .name("shell-open".into())
            .spawn(move || {
                use windows_sys::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
                use windows_sys::Win32::UI::Shell::ShellExecuteW;
                use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
                // A shell extension may be COM-based; this thread has to be initialized
                // before the call. STA, as the ASIO path does; S_FALSE says the thread already
                // was, which is equally usable.
                let hr =
                    unsafe { CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32) };
                if hr < 0 {
                    crate::verr!("[SHELL]  CoInitializeEx failed ({hr:#x}), target not opened");
                    return;
                }
                let verb: Vec<u16> = "open\0".encode_utf16().collect();
                // SAFETY: verb/file are null-terminated UTF-16; the unused pointers are null,
                // which ShellExecuteW documents as "no parameters / default directory".
                unsafe {
                    ShellExecuteW(
                        std::ptr::null_mut(),
                        verb.as_ptr(),
                        file.as_ptr(),
                        std::ptr::null(),
                        std::ptr::null(),
                        SW_SHOWNORMAL,
                    );
                }
            })
            .map_err(|e| crate::verr!("[SHELL]  Could not spawn the shell-open thread: {e}"))
            .ok();
    }
    #[cfg(target_os = "linux")]
    {
        if let Err(e) = std::process::Command::new("xdg-open").arg(target).spawn() {
            crate::verr!("[SHELL]  Could not run xdg-open: {e}");
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Err(e) = std::process::Command::new("open").arg(target).spawn() {
            crate::verr!("[SHELL]  Could not run open: {e}");
        }
    }
}

pub(crate) fn toggle_devtools() {
    let browser = with_state(|state| state.browser.clone());
    if let Some(Some(browser)) = browser
        && let Some(host) = browser.host()
    {
        if host.has_dev_tools() == 1 {
            host.close_dev_tools();
        } else {
            host.show_dev_tools(None, None, None, None);
        }
    }
}

#[cfg(test)]
#[path = "../tests/unit/app_state.rs"]
mod tests;
