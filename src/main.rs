#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
mod app_state;
mod audio;
mod bridge;
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

    {
        let _guard = rt.enter();
        let _ = &*crate::state::GOVERNOR;
        let _ = &*crate::state::AUDIO_CACHE;
    }

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

    let data_dir = state::cache_data_dir();
    if let Err(e) = std::fs::create_dir_all(&data_dir) {
        crate::vprintln!("[DB] Failed to create data dir {}: {e}", data_dir.display());
    }
    let db_actor = db::DbActor::open(&data_dir).expect("Failed to open databases");
    let _ = state::DB.set(db_actor);

    let _ = APP_STATE.set(Arc::new(Mutex::new(AppState {
        player,
        rt_handle,
        pending_time_update: None,
        pending_player_events: Vec::new(),
        pending_misc_js: Vec::new(),
        browser: None,
        flush_scheduled: false,
        media_controls: None,
        media_duration: None,
        plugin_manager: plugins::PluginManager::new(),
        captured_token: String::new(),
        pending_ipc_callbacks: HashMap::new(),
        native_runtime: None,
        pending_window_save: None,
        window_save_scheduled: false,
        #[cfg(target_os = "windows")]
        thumbbar: None,
    })));

    let root_cache = state::cache_data_dir().join("cef");
    let profile_cache = root_cache.join("Default");
    std::fs::create_dir_all(&profile_cache).ok();

    let root_cache_cef = CefString::from(root_cache.to_string_lossy().as_ref());
    let profile_cache_cef = CefString::from(profile_cache.to_string_lossy().as_ref());

    let user_agent = CefString::from(crate::state::USER_AGENT);

    let settings = Settings {
        no_sandbox: 0,
        root_cache_path: root_cache_cef,
        cache_path: profile_cache_cef,
        user_agent,
        background_color: 0xFF111111,
        chrome_app_icon_id: 101,
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
    shutdown();
    Ok(())
}
