use super::UpdateChannel;
use super::download::cleanup_staging;
use super::types::{CheckOutcome, CheckSettlement, StagedVerdict, UPDATER_STATE, UpdateInfo};
use super::util::{detect_staged_update, dev_order_key, fetch_gh_releases, is_newer};

/// Check for updates from GitHub Releases on the given channel.
///
/// Every exit names what it concluded. A caller has to tell "nothing to install" from "the
/// check never got through" before it can know whether an offer already on screen was
/// disproved or merely left unconfirmed.
pub(crate) async fn check_for_update(channel: UpdateChannel) -> CheckOutcome {
    let current_version = env!("CARGO_PKG_VERSION");

    crate::vprintln!(
        "[UPDATER] Checking for updates (current: v{current_version}, channel: {channel:?})..."
    );

    let client = &*crate::state::HTTP_CLIENT;

    // Enumerate releases instead of GitHub's `releases/latest`: the fork
    // inherits upstream releases whose tags outrank ours on raw numbers, and
    // only a release carrying this platform's manifest is installable; the
    // pick is "newest installable", not "newest published".
    let releases = match fetch_gh_releases(client).await {
        Ok(r) => r,
        Err(e) => {
            crate::vprintln!("[UPDATER] Failed to fetch releases: {e}");
            return CheckOutcome::Failed {
                reason: "Could not reach the release list".to_string(),
            };
        }
    };

    let manifest_name = super::manifest_name();
    let release = releases
        .into_iter()
        .filter(|r| !r.draft && (channel == UpdateChannel::Dev || !r.prerelease))
        .filter(|r| r.assets.iter().any(|a| a.name == manifest_name))
        .filter_map(|r| {
            let v = r.tag_name.strip_prefix('v').unwrap_or(&r.tag_name);
            dev_order_key(v).map(|key| (key, r))
        })
        .max_by(|a, b| a.0.cmp(&b.0))
        .map(|(_, r)| r);
    let Some(release) = release else {
        // The list was read: no release on this channel carries this platform's manifest.
        // That answers what is installable rather than failing to find out.
        crate::vprintln!(
            "[UPDATER] No installable release found for {}",
            super::TARGET
        );
        return CheckOutcome::UpToDate;
    };

    // Extract version from tag (strip leading 'v')
    let remote_version = release
        .tag_name
        .strip_prefix('v')
        .unwrap_or(&release.tag_name);

    // Anti-downgrade: only update if remote > current
    if !is_newer(remote_version, current_version) {
        crate::vprintln!("[UPDATER] Up to date (remote: v{remote_version})");
        return CheckOutcome::UpToDate;
    }

    crate::vprintln!("[UPDATER] Update available: v{remote_version}");

    // Find manifest for our platform
    let manifest_name = super::manifest_name();
    let Some(manifest_asset) = release.assets.iter().find(|a| a.name == manifest_name) else {
        // The filter above already required this asset. Losing it here means the release
        // changed under the check, not that there is nothing to install.
        crate::vprintln!("[UPDATER] Manifest {manifest_name} gone from v{remote_version}");
        return CheckOutcome::Failed {
            reason: "The release changed while it was being read".to_string(),
        };
    };

    let manifest_resp = match client
        .get(&manifest_asset.browser_download_url)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            crate::vprintln!("[UPDATER] Failed to download manifest: {e}");
            return CheckOutcome::Failed {
                reason: "Could not download the update manifest".to_string(),
            };
        }
    };

    let manifest_body = match manifest_resp.text().await {
        Ok(t) => t,
        Err(e) => {
            crate::vprintln!("[UPDATER] Failed to read manifest body: {e}");
            return CheckOutcome::Failed {
                reason: "Could not read the update manifest".to_string(),
            };
        }
    };
    let manifest: super::types::Manifest = match serde_json::from_str(&manifest_body) {
        Ok(m) => m,
        Err(e) => {
            crate::vprintln!("[UPDATER] Failed to parse manifest: {e}");
            return CheckOutcome::Failed {
                reason: "The update manifest could not be parsed".to_string(),
            };
        }
    };

    // Verify target matches
    if let Err(e) = manifest.verify_target() {
        crate::vprintln!("[UPDATER] {e}");
        return CheckOutcome::Failed {
            reason: "The update manifest is not for this platform".to_string(),
        };
    }

    // Advisory only - the authoritative gates run post-signature in download.rs;
    // these just avoid offering an update that would be rejected.
    if !super::util::meets_min_version(current_version, &manifest.min_version) {
        crate::vprintln!(
            "[UPDATER] v{remote_version} requires installed >= v{} (have v{current_version}); not offering",
            manifest.min_version
        );
        return CheckOutcome::Withheld {
            reason: format!(
                "v{remote_version} needs v{} or newer installed first",
                manifest.min_version
            ),
        };
    }
    let mark = super::highwater::load(&crate::state::cache_data_dir());
    if !is_newer(&manifest.version, &mark) {
        crate::vprintln!(
            "[UPDATER] v{} not newer than highest installed v{mark}; not offering (anti-rollback)",
            manifest.version
        );
        return CheckOutcome::UpToDate;
    }

    // Linux-only: advisory check that the system bootstrap supports the
    // manifest's required sandbox-helper protocol. Catches the "user has not
    // run apt upgrade yet" case early, sparing bandwidth on a download we'd
    // refuse to apply. The authoritative gate (after signature
    // verification) lives in updater/src/main.rs::run.
    // The reason travels as the outcome, not on a channel of its own: whoever asked reads it
    // in the reply, and the automatic check announces it. A second channel carrying the same
    // string is one that nothing is obliged to listen to.
    #[cfg(target_os = "linux")]
    if let Err(e) = super::util::enforce_sandbox_protocol_gate(&manifest) {
        crate::vprintln!("[UPDATER] {e}");
        return CheckOutcome::Withheld {
            reason: format!("{e}"),
        };
    }

    let archive_name = if manifest.delta_from.as_deref() == Some(current_version) {
        super::delta_archive_name(remote_version)
    } else {
        super::archive_name(remote_version)
    };
    let download_size = release
        .assets
        .iter()
        .find(|a| a.name == archive_name)
        .map(|a| a.size)
        .unwrap_or(0);

    CheckOutcome::Available {
        info: UpdateInfo {
            version: remote_version.to_string(),
            download_size,
        },
    }
}

/// Trigger the update check and notify the frontend if an update is available.
/// Called after login on the tokio runtime.
pub(crate) fn trigger_update_check() {
    // Managed installs (Nix, etc.) are upgraded by the package manager and the
    // updater can't write to the read-only prefix; skip, leaving no toast.
    if crate::util::is_managed_install() {
        crate::vprintln!("[UPDATER] Managed install; in-app updater disabled");
        return;
    }

    // Check settings
    let (auto_check, skip_version, channel) = crate::state::db().call_settings(|conn| {
        (
            crate::settings::load_update_auto_check(conn),
            crate::settings::load_update_skip_version(conn),
            crate::settings::load_update_channel(conn),
        )
    });
    if !auto_check {
        crate::vprintln!("[UPDATER] Auto-check disabled");
        return;
    }
    let channel = UpdateChannel::from_setting(&channel);

    crate::state::rt_handle().spawn(async move {
        // Check if a previous session left a valid pre-downloaded staging
        if let Some(staged_version) = detect_staged_update() {
            // The scan reads the filesystem under no lock, and this runs on every login
            // rather than at boot alone. What it found may belong to an operation this
            // state is already running. `take_staged` is what decides, and it decides both
            // halves: the phase this branch used to write over, and the directory the other
            // half used to delete without asking.
            let verdict = UPDATER_STATE
                .lock()
                .await
                .take_staged(&staged_version, skip_version.as_deref());
            match verdict {
                StagedVerdict::Adopted => {
                    crate::vprintln!(
                        "[UPDATER] Found pre-downloaded staging for v{staged_version}"
                    );
                    crate::app_state::emit_ipc_event_with_args("updater.ready", &[&staged_version]);
                    return;
                }
                StagedVerdict::Declined => {
                    crate::vprintln!(
                        "[UPDATER] Staged v{staged_version} is dismissed, cleaning up"
                    );
                    cleanup_staging();
                }
                // Whatever holds the phase answers for this install already, and it will
                // announce its own end. The release list has nothing to add meanwhile.
                StagedVerdict::Busy => {
                    crate::vprintln!(
                        "[UPDATER] Staged v{staged_version} left to the operation already running"
                    );
                    return;
                }
            }
        }

        let outcome = check_for_update(channel).await;

        // A dismissal is the user's answer rather than the check's: it neither publishes
        // this version nor disproves whatever the record already holds.
        if let CheckOutcome::Available { info } = &outcome
            && let Some(ref skip) = skip_version
            && *skip == info.version
        {
            crate::vprintln!("[UPDATER] Skipping dismissed version v{}", info.version);
            return;
        }

        let settlement = UPDATER_STATE.lock().await.settle_check(&outcome, channel);

        // An answer for a channel the user has left is announced to nobody: it searched the
        // wrong release list, and `updater.channel_changed` has already told every surface to
        // drop what that channel produced. Returning here covers the announcement as well as
        // the record: emitting `updater.available` for a settlement that was refused is how
        // two surfaces come to disagree with the backend they read from.
        if settlement == CheckSettlement::Stale {
            crate::vprintln!("[UPDATER] Check answered for a channel since left; discarding");
            return;
        }

        // This check could announce one outcome out of the four it can reach, the one it
        // found. A newer version a gate refuses went to a channel with no listener, beside a
        // log line gated behind a level nobody runs by default: the user was told nothing,
        // anywhere, and only noticed that updates had stopped arriving.
        match &outcome {
            CheckOutcome::Available { info } => {
                crate::app_state::emit_ipc_event_with_data("updater.available", info);
                crate::vprintln!(
                    "[UPDATER] Notified frontend: v{} ({} bytes)",
                    info.version,
                    info.download_size
                );
            }
            CheckOutcome::Withheld { reason } => {
                crate::app_state::emit_ipc_event_with_args("updater.withheld", &[reason]);
            }
            // Nobody asked for this check; a clean "nothing to install" and a check that
            // could not conclude are both told to no one. The settle above already decided
            // what happens to the offer.
            CheckOutcome::UpToDate | CheckOutcome::Failed { .. } => {}
        }
    });
}
