use std::fs;

// Names the `wrap_task!` expansion refers to unqualified, including the refcount trait.
use cef::rc::Rc as _;
use cef::{ImplTask, Task, WrapTask};
use tokio_util::sync::CancellationToken;

use super::UPDATER_EXE;
use super::check::check_for_update;
use super::download::{cleanup_staging, download_update};
use super::types::{
    CheckOutcome, CheckSettlement, ClaimVerdict, DownloadRefusal, Manifest, StatusResponse,
    UPDATER_STATE, UpdaterPhase,
};
use super::util::exe_dir;

/// Handle `updater.check`: manual check triggered by UI.
///
/// Answers the outcome rather than an `UpdateInfo`-or-null: the surface that asked cannot
/// say "up to date" on a check that never got through, and it settles the shared offer on
/// the way out, for the answer to outlive this reply.
pub(crate) fn handle_updater_check(callback: crate::app_state::IpcCallback) {
    // Managed installs are upgraded by the package manager; a newer version may well
    // exist that this install must not take on its own.
    if crate::util::is_managed_install() {
        reply_check(
            &callback,
            &CheckOutcome::Withheld {
                reason: "Updates for this install come from your package manager".to_string(),
            },
        );
        return;
    }

    crate::state::rt_handle().spawn(async move {
        // Read inside the task, not before it: this handler is entered on the CEF UI thread,
        // where waiting on the actor freezes the window for the round trip.
        let channel = match tokio::task::spawn_blocking(|| {
            crate::state::db().call_settings(crate::settings::load_update_channel)
        })
        .await
        {
            Ok(channel) => channel,
            Err(e) => {
                // Defaulting would silently answer for the stable channel, which is a wrong
                // answer for a dev-channel user rather than no answer.
                crate::verr!("[UPDATER] Channel read failed, check abandoned: {e}");
                reply_check(
                    &callback,
                    &CheckOutcome::Failed {
                        reason: "Could not read the update channel".to_string(),
                    },
                );
                return;
            }
        };
        let channel = super::UpdateChannel::from_setting(&channel);
        let outcome = check_for_update(channel).await;
        // The lock is released before either answer: announcing runs a script on the CEF UI
        // thread, and nothing decided here needs to still be true while it does.
        let settlement = UPDATER_STATE.lock().await.settle_check(&outcome, channel);
        if settlement == CheckSettlement::Stale {
            reply_stale_check(&callback);
            return;
        }
        reply_check(&callback, &outcome);
    });
}

/// Answer a check on the reply protocol: a conclusion comes back as the outcome, a check
/// that reached none comes back as a failure.
///
/// The split matters to the caller's `catch`, which keeps the offer it already has: a
/// dropped connection is not grounds for painting "up to date" over a real update.
fn reply_check(callback: &crate::app_state::IpcCallback, outcome: &CheckOutcome) {
    if let CheckOutcome::Failed { reason } = outcome {
        crate::ipc::plugin::ipc_callback_err(callback, 500, reason);
        return;
    }
    match serde_json::to_string(outcome) {
        Ok(json) => crate::ipc::plugin::ipc_callback_ok(callback, &json),
        Err(e) => {
            crate::verr!("[UPDATER] Check outcome would not serialize: {e}");
            crate::ipc::plugin::ipc_callback_err(callback, 500, "check outcome not serializable");
        }
    }
}

/// Answer a check whose premise is gone: resolved under a channel this state has left, it
/// searched the wrong release list and none of what it found may reach the asker.
///
/// Answered rather than dropped, for the reason every other refusal in this file is: the surface
/// that asked painted a spinner on the click, and a request that returns nothing leaves it there
/// for the session. Keyed on the status code, never on the message, because matching a protocol
/// string by hand is how `applying` once reached a user as the error text "applying".
fn reply_stale_check(callback: &crate::app_state::IpcCallback) {
    crate::vprintln!("[UPDATER] Refused a check answer resolved under a channel since left");
    crate::ipc::plugin::ipc_callback_err(callback, 409, "channel_changed");
}

/// React to `updater.set_channel`: the channel is the input a check resolved the offer from,
/// and an offer outlives its own premise the moment that setting changes.
///
/// The caller states the event and not the consequence: nothing outside this module has to know
/// an offer is derived from a channel. It does hand over `now`, because a check resolved under
/// the old channel may still be awaiting the network, and the state needs the new premise on
/// record to refuse that answer when it lands.
pub(crate) fn channel_changed(now: super::UpdateChannel) {
    crate::state::rt_handle().spawn(async move {
        let staging_must_go = UPDATER_STATE.lock().await.abandon_for_channel_change(now);
        // A staging directory is a few hundred megabytes. Deleting it stays off this task.
        if staging_must_go && let Err(e) = tokio::task::spawn_blocking(cleanup_staging).await {
            crate::verr!("[UPDATER] Staging cleanup after a channel change failed: {e}");
        }
        // Announced, not left to whoever switched: this channel is open to any trusted frame,
        // and a surface still showing the old channel's build would offer a download the
        // backend refuses and an install of files that are gone.
        crate::app_state::emit_ipc_event("updater.channel_changed");
    });
}

/// Answer a refused download, and announce on the same breath the phase it refused into.
///
/// Every arm answers the asker AND says so where every surface reads it, because a reply reaches
/// one caller: the settings page read it, the toast read none of it and went on showing
/// "Downloading update..." with a dead Cancel button for a download that never started.
fn answer_refused_download(
    callback: &crate::app_state::IpcCallback,
    version: &str,
    refusal: DownloadRefusal,
) {
    match refusal {
        DownloadRefusal::NotOnOffer => {
            crate::vprintln!("[UPDATER] Refused download of v{version}: not the current offer");
            crate::ipc::plugin::ipc_callback_err(callback, 403, "not the current offer");
        }
        // Nothing to announce: a surface paints `downloading` on the click, and here that
        // paint is true: a download of this version really is running.
        DownloadRefusal::InProgress => {
            crate::ipc::plugin::ipc_callback_err(callback, 409, "download_in_progress");
        }
        DownloadRefusal::AlreadyReady => {
            crate::ipc::plugin::ipc_callback_ok(callback, "\"already_ready\"");
            crate::app_state::emit_ipc_event_with_args("updater.ready", &[version]);
        }
        DownloadRefusal::Applying => {
            crate::ipc::plugin::ipc_callback_err(callback, 409, "applying");
            crate::app_state::emit_ipc_event("updater.applying");
        }
    }
}

/// Returns immediately with "started", "download_in_progress", "already_ready", or error.
/// Every refusal that names a phase also announces it, for a surface that reads no reply to
/// still be told where it stands.
pub(crate) fn handle_updater_download(
    msg: &crate::app_state::IpcMessage,
    callback: crate::app_state::IpcCallback,
) {
    let version = msg.arg(0).to_string();

    if version.is_empty() {
        crate::ipc::plugin::ipc_callback_err(&callback, 400, "missing version argument");
        return;
    }

    // The package manager owns this install and the updater cannot write to its prefix.
    // `updater.check` refuses for that reason; the two operations that would actually write
    // there had never been asked to.
    if crate::util::is_managed_install() {
        crate::ipc::plugin::ipc_callback_err(
            &callback,
            403,
            "Updates for this install come from your package manager",
        );
        return;
    }

    crate::state::rt_handle().spawn(async move {
        let mut state = UPDATER_STATE.lock().await;

        // Only a version this state itself offered may be fetched, and only while no other
        // operation holds the phase. `answer_download` names which of the two refused, letting
        // the answer below announce the phase rather than merely report the refusal.
        if let Some(refusal) = state.answer_download(&version) {
            // The lock does not cover the answer: announcing runs a script on the CEF UI
            // thread, and nothing under this lock needs to be true while it does.
            drop(state);
            answer_refused_download(&callback, &version, refusal);
            return;
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
        let resp = StatusResponse {
            phase: &state.phase,
            last_info: &state.last_info,
        };
        let json = serde_json::to_string(&resp).unwrap_or_else(|_| "null".into());
        crate::ipc::plugin::ipc_callback_ok(&callback, &json);
    });
}

/// Handle `updater.apply`: user confirmed, spawn updater and quit.
pub(crate) fn handle_updater_apply(msg: &crate::app_state::IpcMessage) {
    let version = msg.arg(0).to_string();

    if version.is_empty() {
        crate::vprintln!("[UPDATER] apply called without version");
        return;
    }

    if crate::util::is_managed_install() {
        crate::verr!("[UPDATER] Refused apply: this install is owned by a package manager");
        crate::app_state::emit_ipc_event_with_args(
            "updater.error",
            &["Updates for this install come from your package manager"],
        );
        return;
    }

    // Both refusals below answer the renderer as well as the log. They return before the task
    // that would emit anything is even spawned, and a click on Apply used to produce no visible
    // effect at all: no spinner, no error, the button still offering a restart that was never
    // going to happen.
    let app_dir = match exe_dir() {
        Some(d) => d,
        None => {
            crate::vprintln!("[UPDATER] Cannot resolve exe dir");
            crate::app_state::emit_ipc_event_with_args(
                "updater.error",
                &["Could not locate the application directory"],
            );
            return;
        }
    };

    let updater_path = app_dir.join(UPDATER_EXE);
    if !updater_path.exists() {
        crate::vprintln!(
            "[UPDATER] Updater binary not found at {}",
            updater_path.display()
        );
        crate::app_state::emit_ipc_event_with_args("updater.error", &["Updater is not installed"]);
        return;
    }

    let pid = std::process::id();

    crate::state::rt_handle().spawn(async move {
        // Read before the claim, and inside the task: this handler is entered on the CEF UI
        // thread, where waiting on the actor freezes the window for the round trip. This is
        // the premise the claim cannot re-derive when it ends: `channel` on the state is
        // `None` until a check settles, and a staged build adopted at boot settles none.
        let claimed_under = match tokio::task::spawn_blocking(|| {
            crate::state::db().call_settings(crate::settings::load_update_channel)
        })
        .await
        {
            Ok(channel) => super::UpdateChannel::from_setting(&channel),
            Err(e) => {
                // Refusing beats claiming a phase whose premise nothing could check when it
                // is handed back, the trade `updater.check` already makes on this read.
                crate::verr!("[UPDATER] Channel read failed, apply abandoned: {e}");
                crate::app_state::emit_ipc_event_with_args(
                    "updater.error",
                    &["Could not read the update channel"],
                );
                return;
            }
        };

        // One apply at a time, and nothing else enforces it: the settings button stays live
        // after the first click, and a plugin can send this message itself. Two race the
        // staging directory and spawn two updater children for the same pid, and on Linux,
        // with no install mutex, both rename over the live install. `Applying` is the phase
        // `handle_updater_download` already refuses to start on top of.
        {
            let mut state = UPDATER_STATE.lock().await;
            match state.phase.claim_apply(&version) {
                ClaimVerdict::Claimed => {}
                ClaimVerdict::InFlight => {
                    crate::vprintln!("[UPDATER] Apply already in flight, ignoring this one");
                    // Answer it anyway. Dropping the request and saying nothing left the
                    // surface that made it showing the state from before the first apply,
                    // still offering a restart that can no longer be claimed. The phase is
                    // the answer: re-stating it costs nothing and settles whoever asked.
                    crate::app_state::emit_ipc_event("updater.applying");
                    return;
                }
                ClaimVerdict::NotStaged => {
                    // The claim refuses a version this state never staged, and that refusal
                    // must not be dressed as an apply that started: the surface would show a
                    // restart on its way for a version no updater child is installing.
                    crate::verr!("[UPDATER] Refused apply of v{version}: nothing staged names it");
                    crate::app_state::emit_ipc_event_with_args(
                        "updater.error",
                        &["That update is no longer the one staged; check for updates again"],
                    );
                    return;
                }
            }
        }

        // What the claim took, kept for the release: the version itself travels into the
        // spawn below, which hands it to a child that outlives this task.
        let claimed_version = version.clone();

        // The claim above is the only place that knows an apply started, and until this event
        // existed the settings button had no way to stop offering one: the guard silently
        // dropped the second click; the only account of the first was that nothing happened.
        crate::app_state::emit_ipc_event("updater.applying");

        crate::vprintln!(
            "[UPDATER] Spawning updater for v{version} (pid={pid}, app_dir={})",
            app_dir.display()
        );

        // Everything below leaves the UI thread. A staged download that no longer matches is a
        // directory of a few hundred megabytes, and deleting it here froze the window BEFORE the
        // exit rather than during it, with nothing to show for the freeze if the spawn then
        // failed and the app kept running.
        let spawned = crate::state::rt_handle()
            .spawn_blocking(move || {
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
                        let mut task = UpdaterQuitTask::new(0);
                        cef::post_task(cef::ThreadId::UI, Some(&mut task));
                        true
                    }
                    Err(e) => {
                        crate::vprintln!("[UPDATER] Failed to spawn updater: {e}");
                        false
                    }
                }
            })
            .await
            .unwrap_or(false);

        if !spawned {
            // The app is still running and no updater child exists, so the claim goes back to
            // the offer it took, or the guard above refuses every later attempt for the rest
            // of the session. Unless the channel moved while it was held: the offer would then
            // be one the switch declined, and the staging spared is owed that switch's deletion.
            let staging_must_go = UPDATER_STATE
                .lock()
                .await
                .release_apply(&claimed_version, claimed_under);
            // A staging directory is a few hundred megabytes. Deleting it stays off this
            // task, as it does on the channel change that deferred it.
            if staging_must_go && let Err(e) = tokio::task::spawn_blocking(cleanup_staging).await {
                crate::verr!("[UPDATER] Staging cleanup after a released claim failed: {e}");
            }
            // And say so, in the same breath. `updater.applying` put the settings button
            // into a disabled loading state on its way in; restoring only the phase this
            // side leaves the renderer holding a restart that is no longer happening, with
            // nothing to tell it otherwise. Announced on the channel the UI already reads
            // for a failed update, because that is what this is.
            crate::app_state::emit_ipc_event_with_args(
                "updater.error",
                &["Could not start the updater"],
            );
        }
    });
}

// The exit half of `updater.apply`, posted back once the updater child is running. Closing
// the window and stopping the message loop are UI-thread-only per CEF's own header, which is
// why the spawn above hands this back instead of finishing inline.
cef::wrap_task! {
    struct UpdaterQuitTask {
        _p: u8,
    }
    impl Task {
        fn execute(&self) {
            crate::app_state::with_state(|state| {
                state.force_quit = true;
            });
            if let Some(window) = crate::ui::app_window::AppWindow::current() {
                window.close();
            } else {
                cef::quit_message_loop();
            }
        }
    }
}

/// Handle `updater.dismiss`: user clicked "Skip this version".
pub(crate) fn handle_updater_dismiss(msg: &crate::app_state::IpcMessage) {
    let version = msg.arg(0).to_string();

    if version.is_empty() {
        return;
    }

    crate::vprintln!("[UPDATER] Dismissed version v{version}");
    let persisted = version.clone();
    crate::state::db().post(move |_, conn| {
        crate::settings::save_update_skip_version(conn, &persisted);
    });

    // The setting was only half of a refusal: it is read by the automatic check alone, while
    // the offer itself lives in memory and is handed back to every surface by
    // `updater.status`. A dismissal that stopped at the setting returned the declined version
    // to the screen on the next mount.
    crate::state::rt_handle().spawn(async move {
        let staging_must_go = UPDATER_STATE.lock().await.dismiss_offer(&version);
        // A staged download is a directory of a few hundred megabytes, and the apply path
        // already pays for deleting one inline.
        if staging_must_go && let Err(e) = tokio::task::spawn_blocking(cleanup_staging).await {
            crate::verr!("[UPDATER] Staging cleanup after a dismissal failed: {e}");
        }
        crate::app_state::emit_ipc_event_with_args("updater.dismissed", &[&version]);
    });
}
