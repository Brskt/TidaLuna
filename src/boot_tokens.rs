use crate::{app_state, platform, vprintln};
use std::sync::Mutex;

/// Boot-token reconcile parked on its own thread while CEF initializes; the
/// join point is on_context_initialized, before the first browser exists.
static BOOT_TOKEN_TASK: Mutex<Option<std::thread::JoinHandle<BootTokenOutcome>>> = Mutex::new(None);

/// Reconcile decision computed off-thread. It never carries disk writes: once
/// initialize() runs, Chromium owns the blob's LevelDB (verified: a join-time
/// write fails on its lock). The only mutation left, purging an unusable blob,
/// is done by the renderer itself.
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
/// browser exists; nothing can observe a half-populated state.
fn restore_session(restored: platform::secure_store::StoredTokenState, needs_refresh: bool) {
    let now_secs = crate::util::now_unix_secs();
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
pub(crate) fn start_boot_token_reconcile(
    data_dir: &std::path::Path,
    cef_profile: &std::path::Path,
) {
    let leveldb_path = cef_profile.join("Local Storage").join("leveldb");

    let stored = match platform::secure_store::load(data_dir) {
        Ok(Some(s)) => s,
        Ok(None) => {
            platform::sdk_storage::purge_sdk_credentials(&leveldb_path);
            vprintln!("[AUTH]   No secure store - purged SDK blob");
            return;
        }
        Err(platform::secure_store::StoreError::Corrupt) => {
            platform::sdk_storage::purge_sdk_credentials(&leveldb_path);
            vprintln!("[AUTH]   Secure store corrupt - purged SDK blob");
            return;
        }
        Err(platform::secure_store::StoreError::Backend(e)) => {
            // Transient (I/O/lock/permission), not corrupt: keep the SDK blob; it
            // re-seeds from the stored token next launch.
            vprintln!("[AUTH]   Secure store backend error (transient) - left intact: {e}");
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
                cur.access_expires_ms(),
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
/// (`NEEDS_BOOT_BLOB_PURGE`), consumed on the first navigation. TIDAL's JS then
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
#[path = "../tests/unit/boot_tokens.rs"]
mod tests;
