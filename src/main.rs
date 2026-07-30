#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
// A bare print reaches neither the LOGS gate nor <data_dir>/console.log: vprintln!
// for what the level may silence, verr! for a failure with no other channel.
// Crate-local, so it does not reach the updater, which has a real console.
#![deny(clippy::print_stderr, clippy::print_stdout)]
mod app_state;
mod audio;
mod bridge;
mod connect;
mod db;
mod debug;
mod ipc;
mod logging;
mod native_runtime;
mod platform;
mod player;
mod plugins;
mod settings;
mod state;
mod ui;
mod updater;
mod util;

use app_state::{APP_STATE, AppState};
use cef::wrapper::message_router::{
    MessageRouterConfig, MessageRouterRendererSide, RendererSideRouter,
};
use cef::*;
use player::{Player, PlayerEvent};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use ui::flush::PlayerEventTask;

/// Populate DISPLAY when launched from a shell without a graphical session env,
/// so the X11 ozone backend can reach the X server (XWayland under a Wayland
/// session, or a native X server).
#[cfg(target_os = "linux")]
fn ensure_x11_env() {
    if std::env::var_os("DISPLAY").is_none() {
        unsafe {
            std::env::set_var("DISPLAY", ":0");
        }
    }
}

#[cfg(target_os = "windows")]
fn attach_or_alloc_console() {
    use windows_sys::Win32::System::Console::{ATTACH_PARENT_PROCESS, AllocConsole, AttachConsole};
    unsafe {
        // Reuse the launching terminal; AllocConsole only when there's no parent.
        if AttachConsole(ATTACH_PARENT_PROCESS) == 0 {
            AllocConsole();
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Seed the live log level from the LOGS env var before anything can log.
    logging::init_env_floor();

    // Reuse the launching terminal's console; AllocConsole only when there's no
    // parent (Explorer). The --type= guard skips CEF subprocesses. This early
    // block is driven by the LOGS env var only (DB isn't open yet).
    #[cfg(target_os = "windows")]
    if logging::log_level() >= 1 && !std::env::args().any(|a| a.starts_with("--type=")) {
        attach_or_alloc_console();
    }

    #[cfg(target_os = "windows")]
    unsafe {
        use windows_sys::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID;
        let app_id: Vec<u16> = "com.tidalunar.app\0".encode_utf16().collect();
        SetCurrentProcessExplicitAppUserModelID(app_id.as_ptr());

        use windows_sys::Win32::System::Registry::{
            HKEY_CURRENT_USER, KEY_WRITE, REG_SZ, RegCreateKeyExW, RegSetValueExW,
        };
        let subkey: Vec<u16> = "Software\\Classes\\AppUserModelId\\com.tidalunar.app\0"
            .encode_utf16()
            .collect();
        let mut hkey = core::ptr::null_mut();
        if RegCreateKeyExW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            0,
            core::ptr::null(),
            0,
            KEY_WRITE,
            core::ptr::null(),
            &mut hkey,
            core::ptr::null_mut(),
        ) == 0
        {
            let name: Vec<u16> = "DisplayName\0".encode_utf16().collect();
            let value: Vec<u16> = "TidaLunar\0".encode_utf16().collect();
            let _ = RegSetValueExW(
                hkey,
                name.as_ptr(),
                0,
                REG_SZ,
                value.as_ptr().cast(),
                (value.len() * 2) as u32,
            );
            windows_sys::Win32::System::Registry::RegCloseKey(hkey);
        }
    }

    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()));

    // Set DLL search directory to bin/cef/ so delay-loaded libcef.dll is found
    #[cfg(target_os = "windows")]
    if let Some(ref dir) = exe_dir {
        let cef_dir = dir.join("bin").join("cef");
        let wide: Vec<u16> = std::os::windows::ffi::OsStrExt::encode_wide(cef_dir.as_os_str())
            .chain(std::iter::once(0))
            .collect();
        unsafe {
            windows_sys::Win32::System::LibraryLoader::SetDllDirectoryW(wide.as_ptr());
        }
    }

    #[cfg(target_os = "linux")]
    ensure_x11_env();

    let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);

    let args = cef::args::Args::new();
    let Some(cmd_line) = args.as_cmd_line() else {
        return Err("Failed to parse command line arguments".into());
    };

    let switch = CefString::from("type");
    let is_browser = cmd_line.has_switch(Some(&switch)) != 1;

    #[cfg(target_os = "linux")]
    if is_browser {
        use std::os::unix::fs::MetadataExt;
        use std::path::PathBuf;

        // Resolution order:
        //   1. CHROME_DEVEL_SANDBOX env (set by /usr/bin/tidalunar.real launcher
        //      under packaged install, points at /opt/tidalunar/bin/cef/chrome-sandbox).
        //   2. exe_dir-relative bin/cef/chrome-sandbox (covers unpackaged dev runs
        //      and the .tar.gz "no system helper" case).
        let sandbox_path: Option<PathBuf> = std::env::var_os("CHROME_DEVEL_SANDBOX")
            .map(PathBuf::from)
            .or_else(|| {
                exe_dir
                    .as_ref()
                    .map(|d| d.join("bin").join("cef").join("chrome-sandbox"))
            });

        if let Some(path) = sandbox_path {
            match std::fs::metadata(&path) {
                Ok(meta) => {
                    let mode = meta.mode();
                    let is_setuid_root = meta.uid() == 0 && (mode & 0o4000) != 0;
                    let is_executable = (mode & 0o111) != 0;
                    // Both are valid:
                    //   - setuid root → legacy SUID sandbox (postinst chmod 4755
                    //     when unprivileged userns is unavailable)
                    //   - normal executable, not setuid → namespace sandbox
                    //     (Chromium picks userns automatically when chrome-sandbox
                    //     is present but not setuid root)
                    // Fail fast only if the file exists but is unreadable / not
                    // executable: that's an administrative misconfiguration we
                    // want loud rather than a silent fallback to no sandbox.
                    // Fatal: must be loud at LOGS=0, and still traced on a desktop
                    // launch where nothing is attached to stderr.
                    if !is_setuid_root && !is_executable {
                        crate::verr!(
                            "chrome-sandbox at {} is neither setuid-root nor a normal executable.",
                            path.display()
                        );
                        crate::verr!("Reinstall the .deb or fix the binary's permissions.");
                        std::process::exit(1);
                    }
                }
                Err(_) => {
                    // Absence is acceptable. .tar.gz installs ship no helper at
                    // all; Chromium falls back to namespace sandbox when userns
                    // is available, or surfaces a clear log message when it
                    // isn't (Ubuntu 24.04+ apparmor restriction without the
                    // .deb's profile). That's the right place for the message,
                    // not here.
                }
            }
        }
    }

    let renderer_config = MessageRouterConfig::default();
    let renderer_router = RendererSideRouter::new(renderer_config);

    let mut app = ui::TidalApp::new(renderer_router);
    let ret = execute_process(
        Some(args.as_main_args()),
        Some(&mut app),
        std::ptr::null_mut(),
    );
    if !is_browser {
        std::process::exit(ret);
    }

    // Single-instance guard, before any DB/SDK/Connect/CEF work: a duplicate would
    // otherwise race (and could purge) the running instance's SDK store. It signals
    // the running window to focus, then exits.
    let _app_lock = match platform::app_lock::acquire_or_signal() {
        Some(lock) => lock,
        None => return Ok(()),
    };

    // Past the subprocess early-exit and the instance lock: only the surviving
    // browser process may touch the desktop entry.
    #[cfg(target_os = "linux")]
    platform::desktop_entry::install();

    // Open the sink early (and rotate last session's log) so early lines are
    // captured; safe before DB init since cache_data_dir() is env-only.
    crate::logging::rotate_console_log(&crate::state::cache_data_dir());
    if crate::logging::log_level() >= 1 {
        crate::logging::ensure_file_sink();
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let rt_handle = rt.handle().clone();
    state::RT_HANDLE
        .set(rt_handle.clone())
        .expect("RT_HANDLE already initialized");

    {
        let _guard = rt.enter();
        // GOVERNOR's init calls tokio::spawn, which needs the runtime entered.
        let _ = &*crate::state::GOVERNOR;
    }

    // Warm the audio cache in parallel with the rest of boot: eager init here
    // delayed first paint, and fully lazy would bill the SQLite open to the
    // first track load or menu open. A racing first use just blocks on the
    // LazyLock until the open finishes.
    if let Err(e) = std::thread::Builder::new()
        .name("cache-warm".into())
        .spawn(|| {
            let _ = &*crate::state::AUDIO_CACHE;
        })
    {
        crate::vprintln!("[CACHE]  Warm thread spawn failed ({e})");
    }

    let data_dir = state::cache_data_dir();
    if let Err(e) = std::fs::create_dir_all(&data_dir) {
        crate::vprintln!("[DB] Failed to create data dir {}: {e}", data_dir.display());
    }
    let db_actor = db::DbActor::open(&data_dir).expect("Failed to open databases");
    let _ = state::DB.set(db_actor);

    // Load the bootstrap settings snapshot once, off the CEF UI thread, as the
    // single source of truth: used here for the early log level + Windows console
    // decision, and later for the init-script globals. Browser-only here, so no
    // --type= guard; the env path opens the console first, so only attach when it
    // didn't (no double-alloc).
    let boot = crate::state::db().call_settings(crate::settings::load_boot_settings);
    let _ = crate::state::BOOT_SETTINGS.set(boot);
    crate::logging::set_log_level(boot.log_level);
    #[cfg(target_os = "windows")]
    if boot.console && crate::logging::log_level() >= 1 && crate::logging::env_log_level() < 1 {
        attach_or_alloc_console();
    }

    // After set_log_level so it's captured whether logging came from the LOGS
    // env or the in-app setting.
    crate::vprintln!("[INIT]   TidaLunar v{}", env!("CARGO_PKG_VERSION"));

    // Recover from any interrupted update before continuing startup
    updater::recover_interrupted_update();

    let player = Arc::new(
        Player::new(
            move |event: PlayerEvent| {
                let mut task = PlayerEventTask::new(event);
                post_task(ThreadId::UI, Some(&mut task));
            },
            rt_handle.clone(),
        )
        .expect("Failed to initialize player"),
    );

    #[cfg(target_os = "windows")]
    crate::player::asio::driver::log_asio_drivers();

    let _ = APP_STATE.set(Arc::new(Mutex::new(AppState {
        player,
        pending_time_update: None,
        pending_player_events: Vec::new(),
        pending_misc_js: Vec::new(),
        browser: None,
        flush_scheduled: false,
        media_controls: None,
        media_duration: None,
        plugin_manager: plugins::PluginManager::new(),
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
        // Gate open by default; cold boot closes it below when a refresh is due.
        proactive_refresh_done: true,
        plugin_load_waiters: Vec::new(),
        last_client_id: String::new(),
        connect: Some(crate::connect::ConnectManager::new()),
    })));

    let root_cache = state::cache_data_dir().join("cef");
    let profile_cache = root_cache.join("Default");
    std::fs::create_dir_all(&profile_cache).ok();

    // Token reconciliation, phase 1: secure-store load + raw SDK-blob read.
    // All LevelDB I/O happens here, while our process is still its only
    // possible opener. The crypto - dominated by a 100k-iteration PBKDF2 -
    // runs on its own thread while CEF initializes; on_context_initialized
    // joins it (finish_boot_tokens) before the first browser exists.
    start_boot_token_reconcile(&data_dir, &profile_cache);

    // Seed the gate mirror before any start path (IPC / SDK) can fire.
    crate::connect::ipc::set_receiver_enabled(boot.receiver_always_on);
    // Spawn the always-on Connect receiver; it doesn't read the boot tokens
    // still reconciling (a casting device brings its own).
    if boot.receiver_always_on
        && let Some(rt) = crate::state::RT_HANDLE.get()
    {
        rt.spawn(crate::connect::ipc::start_receiver_task(
            crate::connect::types::ReceiverConfig::default(),
        ));
    }

    let root_cache_cef = CefString::from(root_cache.to_string_lossy().as_ref());
    let profile_cache_cef = CefString::from(profile_cache.to_string_lossy().as_ref());

    let user_agent = CefString::from(crate::state::USER_AGENT.as_str());

    // CEF resources (.pak, locales, icudtl.dat) live in bin/cef/
    let cef_res_dir = exe_dir
        .as_ref()
        .map(|d| d.join("bin").join("cef"))
        .unwrap_or_default();
    let resources_dir_path = CefString::from(cef_res_dir.to_string_lossy().as_ref());
    // CEF 147 (cef crate 148) wants the locales dir set explicitly.
    let locales_dir_path = CefString::from(cef_res_dir.join("locales").to_string_lossy().as_ref());

    let settings = Settings {
        no_sandbox: 0,
        root_cache_path: root_cache_cef,
        cache_path: profile_cache_cef,
        user_agent,
        background_color: 0xFF111111,
        chrome_app_icon_id: 101,
        resources_dir_path,
        locales_dir_path,
        ..Default::default()
    };

    // initialize() returns 0 both for a process-singleton relaunch (exit cleanly)
    // and for a genuine init failure; the exit code tells them apart.
    if initialize(
        Some(args.as_main_args()),
        Some(&settings),
        Some(&mut app),
        std::ptr::null_mut(),
    ) != 1
    {
        let code = get_exit_code();
        let relaunch =
            cef::sys::cef_resultcode_t::from(Resultcode::NORMAL_EXIT_PROCESS_NOTIFIED) as i32;
        if code == relaunch {
            crate::vprintln!("[CEF]    Another instance owns the session; exiting");
            return Ok(());
        }
        return Err(format!("CEF initialization failed (exit code {code})").into());
    }

    // Record this build's version as the anti-rollback high-water mark now that
    // the app has booted successfully (AVB-style: bump the floor on good boot).
    crate::updater::record_launch_version();

    debug::perf_monitor::start();

    run_message_loop();

    // Stop audio, then tear Connect down on the short exit budget (the window is
    // already gone; a session-grade drain here would just look like a hang).
    let cm = app_state::with_state(|state| {
        let _ = state.player.stop();
        state.connect.take()
    })
    .flatten();
    if let Some(mut cm) = cm
        && let Some(rt) = crate::state::RT_HANDLE.get()
    {
        rt.block_on(cm.shutdown(crate::connect::EXIT_SHUTDOWN_BUDGET));
    }
    crate::connect::bridge::set_active(None);

    shutdown();
    Ok(())
}

/// Boot-token reconcile parked on its own thread while CEF initializes; the
/// join point is on_context_initialized, before the first browser exists.
static BOOT_TOKEN_TASK: Mutex<Option<std::thread::JoinHandle<BootTokenOutcome>>> = Mutex::new(None);

/// Reconcile decision computed off-thread. It never carries disk writes: once
/// initialize() runs, Chromium owns the blob's LevelDB (verified: a join-time
/// write fails on its lock), so the only mutation left - purging an unusable
/// blob - is done by the renderer itself.
enum BootTokenOutcome {
    /// Blob unusable or unrecognized; the renderer purges it before TIDAL's
    /// JS runs (init-script prefix).
    Abandon,
    /// Blob coherent with a stored generation; restore the session.
    /// needs_refresh is true when the blob holds real or previous-generation
    /// tokens: the proactive refresh then mints a fresh generation and
    /// TIDAL's SDK re-persists the blob itself, converging it without a
    /// disk write.
    Restore {
        tokens: Box<platform::secure_store::StoredTokenState>,
        needs_refresh: bool,
    },
}

/// Restore the reconciled session into AppState. Runs before the first
/// browser exists, so nothing can observe a half-populated state.
fn restore_session(restored: platform::secure_store::StoredTokenState, needs_refresh: bool) {
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let token_valid = restored.current.access_expires > now_secs;
    app_state::with_state(|state| {
        if token_valid {
            state.captured_token = restored.current.access_token.clone();
        }
        state.token_state = Some(restored);
        state.needs_proactive_refresh = needs_refresh;
        state.needs_blob_purge = needs_refresh;
        // Close the plugin-load gate only when a proactive refresh will run.
        state.proactive_refresh_done = !needs_refresh;
    });
}

/// Token reconciliation phase 1, before CEF init: load the secure store, read
/// the raw SDK blob, and run every path that must write the LevelDB (purges,
/// seeding) - our process is still its only possible opener here. Only the
/// read-path crypto goes to the boot-tokens thread.
fn start_boot_token_reconcile(data_dir: &std::path::Path, cef_profile: &std::path::Path) {
    let leveldb_path = cef_profile.join("Local Storage").join("leveldb");

    let stored = match platform::secure_store::load(data_dir) {
        Ok(Some(s)) => s,
        Ok(None) | Err(platform::secure_store::StoreError::Unavailable) => {
            platform::sdk_storage::purge_sdk_credentials(&leveldb_path);
            vprintln!("[AUTH]   No secure store - purged SDK blob");
            return;
        }
        Err(platform::secure_store::StoreError::Corrupt) => {
            platform::sdk_storage::purge_sdk_credentials(&leveldb_path);
            vprintln!("[AUTH]   Secure store corrupt - purged SDK blob");
            return;
        }
        Err(platform::secure_store::StoreError::Backend) => {
            // Transient (I/O/lock/permission), not corrupt: keep the SDK blob; it
            // re-seeds from the stored token next launch.
            vprintln!("[AUTH]   Secure store backend error (transient) - left intact");
            return;
        }
    };

    use platform::sdk_storage::ReadRawResult;
    let raw = match platform::sdk_storage::read_raw_blob(&leveldb_path) {
        ReadRawResult::Missing => {
            // TIDAL's SDK validates JWT format - opaque tokens fail validation
            // and trigger session_clear. Seed with REAL tokens instead.
            // The blob is AES-256 encrypted and plugins can't access localStorage.
            // Synchronous - PBKDF2 included - because this write must land
            // before initialize().
            vprintln!("[AUTH]   No SDK storage - seeding from secure store");
            let cur = &stored.current;
            let seeded = platform::sdk_storage::build_seed_entries(
                &cur.access_token,
                &cur.refresh_token,
                cur.access_expires,
                cur.user_id.as_deref(),
                &cur.granted_scopes,
            )
            .and_then(|entries| platform::sdk_storage::write_entries(&leveldb_path, &entries))
            .is_some();
            if seeded {
                vprintln!("[AUTH]   SDK blob seeded successfully");
                restore_session(stored, true);
            } else {
                vprintln!("[AUTH]   SDK blob seeding failed");
            }
            return;
        }
        ReadRawResult::Raw(raw) => raw,
        ReadRawResult::Corrupt => {
            platform::sdk_storage::purge_sdk_credentials(&leveldb_path);
            vprintln!("[AUTH]   SDK storage corrupt - purged");
            return;
        }
        ReadRawResult::Unreadable => {
            // Likely locked, not corrupt: leave the blob intact, don't purge.
            vprintln!("[AUTH]   SDK storage unreadable (locked?) - left intact");
            return;
        }
    };

    match std::thread::Builder::new()
        .name("boot-tokens".into())
        .spawn(move || reconcile_sdk_blob(raw, stored))
    {
        Ok(handle) => {
            *BOOT_TOKEN_TASK.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);
        }
        Err(e) => {
            // Same degradation as a transient backend error: nothing purged,
            // the next launch reconciles.
            vprintln!("[AUTH]   Boot token thread spawn failed ({e}) - left intact");
        }
    }
}

/// Token reconciliation phase 2, on the boot-tokens thread: the read-path
/// crypto (the 100k-iteration PBKDF2, AES) plus the match against the stored
/// generations. Pure CPU - no I/O of any kind.
fn reconcile_sdk_blob(
    raw: Box<platform::sdk_storage::RawSdkBlob>,
    stored: platform::secure_store::StoredTokenState,
) -> BootTokenOutcome {
    let Some(credentials) = platform::sdk_storage::decrypt_raw_blob(&raw) else {
        vprintln!("[AUTH]   SDK storage corrupt - purging in renderer");
        return BootTokenOutcome::Abandon;
    };
    let sdk_at = credentials
        .access_token
        .as_ref()
        .and_then(|a| a.token.as_deref())
        .unwrap_or("")
        .to_string();
    let sdk_rt = credentials.refresh_token.unwrap_or_default();

    // Match against opaque tokens (normal flow)
    if sdk_at == stored.current.opaque_at && sdk_rt == stored.current.opaque_rt {
        vprintln!("[AUTH]   Boot reconciliation: current match (opaque)");
        return BootTokenOutcome::Restore {
            tokens: Box::new(stored),
            needs_refresh: false,
        };
    }

    // Match against real tokens (seeded blob - TIDAL re-persisted them)
    if sdk_at == stored.current.access_token && sdk_rt == stored.current.refresh_token {
        vprintln!("[AUTH]   Boot reconciliation: current match (real)");
        return BootTokenOutcome::Restore {
            tokens: Box::new(stored),
            needs_refresh: true,
        };
    }

    // One opaque generation behind: restore on the previous mapping and let
    // the proactive refresh mint a fresh generation; TIDAL's SDK re-persists
    // the blob itself (an in-place disk rewrite would race Chromium here).
    if let Some(ref prev) = stored.previous
        && sdk_at == prev.opaque_at
        && sdk_rt == prev.opaque_rt
    {
        vprintln!("[AUTH]   Boot reconciliation: previous match (opaque) - refreshing to converge");
        return BootTokenOutcome::Restore {
            tokens: Box::new(stored),
            needs_refresh: true,
        };
    }

    // Match previous generation against real tokens too
    if let Some(ref prev) = stored.previous
        && sdk_at == prev.access_token
        && sdk_rt == prev.refresh_token
    {
        vprintln!("[AUTH]   Boot reconciliation: previous match (real)");
        return BootTokenOutcome::Restore {
            tokens: Box::new(stored),
            needs_refresh: true,
        };
    }

    vprintln!("[AUTH]   Boot reconciliation: no match - purging in renderer");
    BootTokenOutcome::Abandon
}

/// Token reconciliation phase 3: join the boot-tokens thread and apply its
/// decision. Called from on_context_initialized before the init script is
/// built: a restored session must be in AppState before the page can consume
/// it. On an unusable blob it arms a one-shot renderer purge
/// (`NEEDS_BOOT_BLOB_PURGE`), consumed on the first navigation, so TIDAL's JS
/// starts from a clean localStorage.
pub(crate) fn finish_boot_tokens() {
    let task = BOOT_TOKEN_TASK
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take();
    let Some(handle) = task else { return };
    let Ok(outcome) = handle.join() else {
        // A panic in the reconcile thread degrades like a transient backend
        // error: nothing purged, the next launch reconciles.
        vprintln!("[AUTH]   Boot token reconcile panicked - left intact");
        return;
    };
    match outcome {
        BootTokenOutcome::Abandon => {
            crate::ui::NEEDS_BOOT_BLOB_PURGE.store(true, std::sync::atomic::Ordering::Release);
        }
        BootTokenOutcome::Restore {
            tokens,
            needs_refresh,
        } => restore_session(*tokens, needs_refresh),
    }
}

#[cfg(test)]
mod boot_token_tests {
    use super::*;
    use platform::sdk_storage::{self, RawSdkBlob};
    use platform::secure_store::{StoredTokenState, TokenGeneration};

    fn generation(tag: &str) -> TokenGeneration {
        TokenGeneration {
            access_token: format!("real_at_{tag}"),
            refresh_token: format!("real_rt_{tag}"),
            opaque_at: format!("luna_at_{tag}"),
            opaque_rt: format!("luna_rt_{tag}"),
            version: 1,
            access_expires: u64::MAX,
            user_id: Some("42".into()),
            granted_scopes: vec!["r_usr".into()],
            client_id: "cid".into(),
        }
    }

    fn stored(current: TokenGeneration, previous: Option<TokenGeneration>) -> StoredTokenState {
        StoredTokenState {
            current,
            previous,
            previous_valid_until: None,
        }
    }

    /// Rebuild the RawSdkBlob a LevelDB read would produce from seed entries.
    fn raw_from_entries(entries: sdk_storage::SdkEntries) -> Box<RawSdkBlob> {
        let mut salt = None;
        let mut counter = None;
        let mut wrapped_key = None;
        let mut data = None;
        for (key, value) in entries {
            match key {
                "AuthDB/tidalSalt" => salt = Some(value),
                "AuthDB/tidalCounter" => counter = Some(value),
                "AuthDB/tidalKey" => wrapped_key = Some(value),
                "AuthDB/tidalData" => data = Some(value),
                other => panic!("unexpected entry {other}"),
            }
        }
        Box::new(RawSdkBlob {
            salt: salt.unwrap().try_into().unwrap(),
            counter: counter.unwrap().try_into().unwrap(),
            wrapped_key: wrapped_key.unwrap(),
            data: data.unwrap(),
        })
    }

    /// A blob holding `at`/`rt`, built through the same path the seed uses.
    fn blob_with(at: &str, rt: &str) -> Box<RawSdkBlob> {
        let entries =
            sdk_storage::build_seed_entries(at, rt, u64::MAX, Some("42"), &["r_usr".to_string()])
                .expect("seed entries");
        raw_from_entries(entries)
    }

    fn blob_tokens(raw: &RawSdkBlob) -> (String, String) {
        let credentials = sdk_storage::decrypt_raw_blob(raw).expect("blob decrypts");
        let at = credentials
            .access_token
            .and_then(|a| a.token)
            .unwrap_or_default();
        let rt = credentials.refresh_token.unwrap_or_default();
        (at, rt)
    }

    #[test]
    fn seed_entries_roundtrip_through_decrypt() {
        let raw = blob_with("at_value", "rt_value");
        let (at, rt) = blob_tokens(&raw);
        assert_eq!(at, "at_value");
        assert_eq!(rt, "rt_value");
    }

    #[test]
    fn current_opaque_match_restores_without_refresh() {
        let cur = generation("cur");
        let raw = blob_with(&cur.opaque_at, &cur.opaque_rt);
        let BootTokenOutcome::Restore { needs_refresh, .. } =
            reconcile_sdk_blob(raw, stored(cur, None))
        else {
            panic!("expected Restore");
        };
        assert!(!needs_refresh);
    }

    #[test]
    fn current_real_match_needs_refresh() {
        let cur = generation("cur");
        let raw = blob_with(&cur.access_token, &cur.refresh_token);
        let BootTokenOutcome::Restore { needs_refresh, .. } =
            reconcile_sdk_blob(raw, stored(cur, None))
        else {
            panic!("expected Restore");
        };
        assert!(needs_refresh);
    }

    #[test]
    fn previous_opaque_match_restores_with_refresh() {
        let prev = generation("old");
        let cur = generation("new");
        let raw = blob_with(&prev.opaque_at, &prev.opaque_rt);
        let BootTokenOutcome::Restore { needs_refresh, .. } =
            reconcile_sdk_blob(raw, stored(cur, Some(prev)))
        else {
            panic!("expected Restore");
        };
        assert!(needs_refresh);
    }

    #[test]
    fn previous_real_match_restores_with_refresh() {
        let prev = generation("old");
        let cur = generation("new");
        let raw = blob_with(&prev.access_token, &prev.refresh_token);
        let BootTokenOutcome::Restore { needs_refresh, .. } =
            reconcile_sdk_blob(raw, stored(cur, Some(prev)))
        else {
            panic!("expected Restore");
        };
        assert!(needs_refresh);
    }

    #[test]
    fn unknown_blob_abandons() {
        let raw = blob_with("stranger_at", "stranger_rt");
        let outcome = reconcile_sdk_blob(raw, stored(generation("cur"), Some(generation("old"))));
        assert!(matches!(outcome, BootTokenOutcome::Abandon));
    }

    #[test]
    fn corrupt_blob_abandons() {
        let mut raw = blob_with("at", "rt");
        raw.wrapped_key[0] ^= 0xFF;
        let outcome = reconcile_sdk_blob(raw, stored(generation("cur"), None));
        assert!(matches!(outcome, BootTokenOutcome::Abandon));
    }
}
