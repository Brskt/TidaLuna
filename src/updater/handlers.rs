use std::fs;

use cef::ImplWindow;
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use super::UPDATER_EXE;
use super::check::check_for_update;
use super::download::{cleanup_staging, download_update};
use super::types::{Manifest, UPDATER_STATE, UpdateInfo, UpdaterPhase};
use super::util::exe_dir;

/// Handle `updater.check` — manual check triggered by UI.
/// Returns JSON UpdateInfo or null.
pub(crate) fn handle_updater_check(callback: crate::app_state::IpcCallback) {
    crate::state::rt_handle().spawn(async move {
        let result = match check_for_update().await {
            Some(info) => serde_json::to_string(&info).unwrap_or_else(|_| "null".into()),
            None => "null".into(),
        };
        crate::ipc::plugin::ipc_callback_ok(&callback, &result);
    });
}

/// Returns immediately with "started", "download_in_progress", "already_ready", or error.
pub(crate) fn handle_updater_download(
    msg: &crate::app_state::IpcMessage,
    callback: crate::app_state::IpcCallback,
) {
    let version = msg.arg(0).to_string();

    if version.is_empty() {
        crate::ipc::plugin::ipc_callback_err(&callback, "missing version argument");
        return;
    }

    crate::state::rt_handle().spawn(async move {
        let mut state = UPDATER_STATE.lock().await;
        match &state.phase {
            UpdaterPhase::Downloading(_) => {
                crate::ipc::plugin::ipc_callback_err(&callback, "download_in_progress");
                return;
            }
            UpdaterPhase::Ready(v) if *v == version => {
                crate::ipc::plugin::ipc_callback_ok(&callback, "\"already_ready\"");
                return;
            }
            UpdaterPhase::Applying(_) => {
                crate::ipc::plugin::ipc_callback_err(&callback, "applying");
                return;
            }
            _ => {}
        }

        if let UpdaterPhase::Ready(_) = &state.phase {
            cleanup_staging();
        }

        let token = CancellationToken::new();
        state.phase = UpdaterPhase::Downloading(version.clone());
        state.cancel = Some(token.clone());

        let handle = tokio::spawn(download_update(version, token));
        state.task = Some(handle);
        drop(state);

        crate::ipc::plugin::ipc_callback_ok(&callback, "\"started\"");
    });
}

pub(crate) fn handle_updater_cancel() {
    crate::state::rt_handle().spawn(async {
        let mut state = UPDATER_STATE.lock().await;
        if let UpdaterPhase::Downloading(_) = &state.phase {
            if let Some(token) = state.cancel.take() {
                token.cancel();
            }
            if let Some(handle) = state.task.take() {
                handle.abort();
            }
            state.phase = UpdaterPhase::Idle;
            cleanup_staging();
            crate::app_state::emit_ipc_event("updater.cancelled");
            crate::vprintln!("[UPDATER] Download cancelled by user");
        }
    });
}

pub(crate) fn handle_updater_status(callback: crate::app_state::IpcCallback) {
    crate::state::rt_handle().spawn(async move {
        let state = UPDATER_STATE.lock().await;
        #[derive(Serialize)]
        struct StatusResponse<'a> {
            #[serde(flatten)]
            phase: &'a UpdaterPhase,
            last_info: &'a Option<UpdateInfo>,
        }
        let resp = StatusResponse {
            phase: &state.phase,
            last_info: &state.last_info,
        };
        let json = serde_json::to_string(&resp).unwrap_or_else(|_| "null".into());
        crate::ipc::plugin::ipc_callback_ok(&callback, &json);
    });
}

/// Handle `updater.apply` — user confirmed, spawn updater and quit.
pub(crate) fn handle_updater_apply(msg: &crate::app_state::IpcMessage) {
    let version = msg.arg(0).to_string();

    if version.is_empty() {
        crate::vprintln!("[UPDATER] apply called without version");
        return;
    }

    let app_dir = match exe_dir() {
        Some(d) => d,
        None => {
            crate::vprintln!("[UPDATER] Cannot resolve exe dir");
            return;
        }
    };

    let updater_path = app_dir.join(UPDATER_EXE);
    if !updater_path.exists() {
        crate::vprintln!(
            "[UPDATER] Updater binary not found at {}",
            updater_path.display()
        );
        return;
    }

    let pid = std::process::id();

    crate::vprintln!(
        "[UPDATER] Spawning updater for v{version} (pid={pid}, app_dir={})",
        app_dir.display()
    );

    let manifest_name = super::manifest_name();
    let staging_manifest_path = app_dir.join(".update-staging").join(&manifest_name);
    let skip_download = match fs::read_to_string(&staging_manifest_path) {
        Ok(data) => match serde_json::from_str::<Manifest>(&data) {
            Ok(m) => m.version == version && m.verify_target().is_ok(),
            Err(_) => false,
        },
        Err(_) => false,
    };

    if !skip_download {
        let staging = app_dir.join(".update-staging");
        if staging.exists() {
            fs::remove_dir_all(&staging).ok();
        }
    }

    let mut cmd = std::process::Command::new(&updater_path);
    cmd.args([
        "--pid",
        &pid.to_string(),
        "--version",
        &version,
        "--app-dir",
        &app_dir.display().to_string(),
    ]);
    if skip_download {
        cmd.arg("--skip-download");
        crate::vprintln!("[UPDATER] Using pre-downloaded staging");
    }

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    match cmd.spawn() {
        Ok(_) => {
            crate::vprintln!("[UPDATER] Updater spawned, exiting app...");
            crate::app_state::with_state(|state| {
                state.force_quit = true;
            });
            let browser = crate::app_state::with_state(|state| state.browser.clone()).flatten();
            if let Some(window) = crate::ipc::window::get_cef_window(browser) {
                window.close();
            } else {
                cef::quit_message_loop();
            }
        }
        Err(e) => {
            crate::vprintln!("[UPDATER] Failed to spawn updater: {e}");
        }
    }
}

/// Handle `updater.dismiss` — user clicked "Skip this version".
pub(crate) fn handle_updater_dismiss(msg: &crate::app_state::IpcMessage) {
    let version = msg.arg(0).to_string();

    if !version.is_empty() {
        crate::vprintln!("[UPDATER] Dismissed version v{version}");
        crate::state::db().call_settings(move |conn| {
            crate::settings::save_update_skip_version(conn, &version);
        });
    }
}
