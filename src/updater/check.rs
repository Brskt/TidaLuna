use super::download::cleanup_staging;
use super::types::{UPDATER_STATE, UpdateInfo, UpdaterPhase};
use super::util::{detect_staged_update, fetch_gh_release, is_newer};

/// Check for updates from GitHub Releases. Returns Some(UpdateInfo) if a newer
/// version is available, None otherwise.
pub(crate) async fn check_for_update() -> Option<UpdateInfo> {
    let current_version = env!("CARGO_PKG_VERSION");

    crate::vprintln!("[UPDATER] Checking for updates (current: v{current_version})...");

    let client = &*crate::state::HTTP_CLIENT;

    // Fetch latest release (log errors explicitly, don't silently swallow them)
    let release = match fetch_gh_release(client, "releases/latest").await {
        Ok(r) => r,
        Err(e) => {
            crate::vprintln!("[UPDATER] Failed to fetch releases: {e}");
            return None;
        }
    };

    // Extract version from tag (strip leading 'v')
    let remote_version = release
        .tag_name
        .strip_prefix('v')
        .unwrap_or(&release.tag_name);

    // Anti-downgrade: only update if remote > current
    if !is_newer(remote_version, current_version) {
        crate::vprintln!("[UPDATER] Up to date (remote: v{remote_version})");
        return None;
    }

    crate::vprintln!("[UPDATER] Update available: v{remote_version}");

    // Find manifest for our platform
    let manifest_name = super::manifest_name();
    let manifest_asset = release.assets.iter().find(|a| a.name == manifest_name)?;

    let manifest_resp = match client
        .get(&manifest_asset.browser_download_url)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            crate::vprintln!("[UPDATER] Failed to download manifest: {e}");
            return None;
        }
    };

    let manifest_body = match manifest_resp.text().await {
        Ok(t) => t,
        Err(e) => {
            crate::vprintln!("[UPDATER] Failed to read manifest body: {e}");
            return None;
        }
    };
    let manifest: super::types::Manifest = match serde_json::from_str(&manifest_body) {
        Ok(m) => m,
        Err(e) => {
            crate::vprintln!("[UPDATER] Failed to parse manifest: {e}");
            return None;
        }
    };

    // Verify target matches
    if let Err(e) = manifest.verify_target() {
        crate::vprintln!("[UPDATER] {e}");
        return None;
    }

    let zip_name = super::zip_name();
    let download_size = release
        .assets
        .iter()
        .find(|a| a.name == zip_name)
        .map(|a| a.size)
        .unwrap_or(0);

    Some(UpdateInfo {
        version: remote_version.to_string(),
        download_size,
    })
}

/// Trigger the update check and notify the frontend if an update is available.
/// Called after login on the tokio runtime.
pub(crate) fn trigger_update_check() {
    // Check settings
    let (auto_check, skip_version) = crate::state::db().call_settings(|conn| {
        (
            crate::settings::load_update_auto_check(conn),
            crate::settings::load_update_skip_version(conn),
        )
    });
    if !auto_check {
        crate::vprintln!("[UPDATER] Auto-check disabled");
        return;
    }

    crate::state::rt_handle().spawn(async move {
        // Check if a previous session left a valid pre-downloaded staging
        if let Some(staged_version) = detect_staged_update() {
            if let Some(ref skip) = skip_version
                && *skip == staged_version
            {
                crate::vprintln!("[UPDATER] Staged v{staged_version} is dismissed, cleaning up");
                cleanup_staging();
            } else {
                let mut state = UPDATER_STATE.lock().await;
                state.phase = UpdaterPhase::Ready(staged_version.clone());
                drop(state);
                crate::vprintln!("[UPDATER] Found pre-downloaded staging for v{staged_version}");
                crate::app_state::emit_ipc_event_with_args("updater.ready", &[&staged_version]);
                return;
            }
        }

        if let Some(info) = check_for_update().await {
            if let Some(ref skip) = skip_version
                && *skip == info.version
            {
                crate::vprintln!("[UPDATER] Skipping dismissed version v{}", info.version);
                return;
            }

            UPDATER_STATE.lock().await.last_info = Some(info.clone());

            crate::app_state::emit_ipc_event_with_data("updater.available", &info);
            crate::vprintln!(
                "[UPDATER] Notified frontend: v{} ({} bytes)",
                info.version,
                info.download_size
            );
        }
    });
}
