#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
mod app_state;
mod audio;
mod bridge;
mod connect;
mod db;
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

fn main() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(target_os = "windows")]
    if logging::log_level() >= 1 {
        unsafe {
            windows_sys::Win32::System::Console::AllocConsole();
        }
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

    let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);

    let args = cef::args::Args::new();
    let Some(cmd_line) = args.as_cmd_line() else {
        return Err("Failed to parse command line arguments".into());
    };

    let switch = CefString::from("type");
    let is_browser = cmd_line.has_switch(Some(&switch)) != 1;

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
        let _ = &*crate::state::GOVERNOR;
        let _ = &*crate::state::AUDIO_CACHE;
    }

    let data_dir = state::cache_data_dir();
    if let Err(e) = std::fs::create_dir_all(&data_dir) {
        crate::vprintln!("[DB] Failed to create data dir {}: {e}", data_dir.display());
    }
    let db_actor = db::DbActor::open(&data_dir).expect("Failed to open databases");
    let _ = state::DB.set(db_actor);

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
        last_client_id: String::new(),
        connect: Some(crate::connect::ConnectManager::new()),
    })));

    let root_cache = state::cache_data_dir().join("cef");
    let profile_cache = root_cache.join("Default");
    std::fs::create_dir_all(&profile_cache).ok();

    if let Some((restored, needs_refresh)) = reconcile_boot_tokens(&data_dir, &profile_cache) {
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
        });
    }

    let receiver_always_on =
        crate::state::db().call_settings(crate::settings::load_receiver_always_on);
    if receiver_always_on && let Some(rt) = crate::state::RT_HANDLE.get() {
        rt.spawn(async move {
            let mut cm = app_state::with_state(|state| state.connect.take()).flatten();
            if let Some(ref mut cm) = cm
                && let Err(e) = cm
                    .start_receiver(crate::connect::types::ReceiverConfig::default())
                    .await
            {
                crate::vprintln!("[connect] Boot auto-start failed: {}", e);
            }
            app_state::with_state(|state| {
                state.connect = cm;
            });
        });
    }

    let root_cache_cef = CefString::from(root_cache.to_string_lossy().as_ref());
    let profile_cache_cef = CefString::from(profile_cache.to_string_lossy().as_ref());

    let user_agent = CefString::from(crate::state::USER_AGENT);

    // CEF resources (.pak, locales, icudtl.dat) live in bin/cef/
    let cef_res_dir = exe_dir
        .as_ref()
        .map(|d| d.join("bin").join("cef"))
        .unwrap_or_default();
    let resources_dir_path = CefString::from(cef_res_dir.to_string_lossy().as_ref());

    let settings = Settings {
        no_sandbox: 0,
        root_cache_path: root_cache_cef,
        cache_path: profile_cache_cef,
        user_agent,
        background_color: 0xFF111111,
        chrome_app_icon_id: 101,
        resources_dir_path,
        ..Default::default()
    };

    assert_eq!(
        initialize(
            Some(args.as_main_args()),
            Some(&settings),
            Some(&mut app),
            std::ptr::null_mut(),
        ),
        1,
        "CEF initialization failed"
    );

    run_message_loop();

    // Shutdown Connect before CEF shutdown
    app_state::with_state(|state| {
        if let Some(ref mut cm) = state.connect {
            // Use the runtime to run async shutdown
            if let Some(rt) = crate::state::RT_HANDLE.get() {
                rt.block_on(cm.shutdown());
            }
        }
        state.connect = None;
    });
    crate::connect::bridge::set_active(None);

    shutdown();
    Ok(())
}

/// Returns (token_state, needs_proactive_refresh).
/// needs_proactive_refresh is true when the SDK blob was seeded with real tokens
/// (TIDAL will have real JWTs in memory until we push opaques).
fn reconcile_boot_tokens(
    data_dir: &std::path::Path,
    cef_profile: &std::path::Path,
) -> Option<(platform::secure_store::StoredTokenState, bool)> {
    let leveldb_path = cef_profile.join("Local Storage").join("leveldb");

    let stored = match platform::secure_store::load(data_dir) {
        Ok(Some(s)) => s,
        Ok(None) | Err(platform::secure_store::StoreError::Unavailable) => {
            platform::sdk_storage::purge_sdk_credentials(&leveldb_path);
            vprintln!("[AUTH]   No secure store - purged SDK blob");
            return None;
        }
        Err(platform::secure_store::StoreError::Corrupt) => {
            platform::sdk_storage::purge_sdk_credentials(&leveldb_path);
            vprintln!("[AUTH]   Secure store corrupt - purged SDK blob");
            return None;
        }
        Err(_) => {
            platform::sdk_storage::purge_sdk_credentials(&leveldb_path);
            vprintln!("[AUTH]   Secure store error - purged SDK blob");
            return None;
        }
    };

    use platform::sdk_storage::{ReadSdkResult, read_sdk_credentials};
    let sdk_result = read_sdk_credentials(&leveldb_path);
    let (sdk_at, sdk_rt, sdk_crypto, sdk_plaintext) = match sdk_result {
        ReadSdkResult::Missing => {
            // TIDAL's SDK validates JWT format - opaque tokens fail validation
            // and trigger session_clear. Seed with REAL tokens instead.
            // The blob is AES-256 encrypted and plugins can't access localStorage.
            vprintln!("[AUTH]   No SDK storage - seeding from secure store");
            let cur = &stored.current;
            if platform::sdk_storage::create_sdk_credentials(
                &leveldb_path,
                &cur.access_token,
                &cur.refresh_token,
                cur.access_expires,
                cur.user_id.as_deref(),
                &cur.granted_scopes,
            )
            .is_some()
            {
                vprintln!("[AUTH]   SDK blob seeded successfully");
                return Some((stored, true));
            }
            vprintln!("[AUTH]   SDK blob seeding failed");
            return None;
        }
        ReadSdkResult::Corrupt => {
            platform::sdk_storage::purge_sdk_credentials(&leveldb_path);
            vprintln!("[AUTH]   SDK storage corrupt - purged");
            return None;
        }
        ReadSdkResult::Parsed {
            credentials,
            crypto,
            plaintext,
        } => {
            let at = credentials
                .access_token
                .as_ref()
                .and_then(|a| a.token.as_deref())
                .unwrap_or("")
                .to_string();
            let rt = credentials.refresh_token.unwrap_or_default();
            (at, rt, crypto, plaintext)
        }
    };

    // Match against opaque tokens (normal flow)
    if sdk_at == stored.current.opaque_at && sdk_rt == stored.current.opaque_rt {
        vprintln!("[AUTH]   Boot reconciliation: current match (opaque)");
        return Some((stored, false));
    }

    // Match against real tokens (seeded blob - TIDAL re-persisted them)
    if sdk_at == stored.current.access_token && sdk_rt == stored.current.refresh_token {
        vprintln!("[AUTH]   Boot reconciliation: current match (real)");
        return Some((stored, true));
    }

    if let Some(ref prev) = stored.previous
        && sdk_at == prev.opaque_at
        && sdk_rt == prev.opaque_rt
    {
        vprintln!("[AUTH]   Boot reconciliation: previous match, rewriting SDK blob");
        if platform::sdk_storage::rewrite_sdk_credentials(
            &leveldb_path,
            &sdk_plaintext,
            &platform::sdk_storage::RewriteFields {
                opaque_at: &stored.current.opaque_at,
                opaque_rt: Some(&stored.current.opaque_rt),
                expires: Some(stored.current.access_expires),
                user_id: stored.current.user_id.as_deref(),
                granted_scopes: Some(&stored.current.granted_scopes),
            },
            &sdk_crypto,
        )
        .is_some()
        {
            return Some((stored, false));
        }
        vprintln!("[AUTH]   SDK rewrite failed - purging");
        platform::sdk_storage::purge_sdk_credentials(&leveldb_path);
        return None;
    }

    // Match previous generation against real tokens too
    if let Some(ref prev) = stored.previous
        && sdk_at == prev.access_token
        && sdk_rt == prev.refresh_token
    {
        vprintln!("[AUTH]   Boot reconciliation: previous match (real)");
        return Some((stored, true));
    }

    vprintln!("[AUTH]   Boot reconciliation: no match - purging");
    platform::sdk_storage::purge_sdk_credentials(&leveldb_path);
    None
}
