//! Tests for `src/updater/types.rs`, attached to it by `#[path]`.

use super::*;

/// A second apply, while the first is still validating staging and spawning, used to be
/// accepted: two workers then deleted the same staging directory and spawned two updater
/// children for one pid. On Windows the updater's own install mutex makes the loser
/// abort; on Linux nothing does, and both rename over the live install. Reaching it needs
/// no plugin: the settings button stays clickable after the first click.
#[test]
fn a_second_apply_is_refused_while_the_first_is_in_flight() {
    let mut phase = UpdaterPhase::Ready("0.0.17-alpha".to_string());

    assert_eq!(
        phase.claim_apply("0.0.17-alpha"),
        ClaimVerdict::Claimed,
        "the first apply takes the slot"
    );
    assert!(matches!(phase, UpdaterPhase::Applying(_)));

    assert_eq!(
        phase.claim_apply("0.0.17-alpha"),
        ClaimVerdict::InFlight,
        "a second updater child would race the first over the live install"
    );
}

fn state_holding(phase: UpdaterPhase, last_info: Option<UpdateInfo>) -> UpdaterState {
    UpdaterState {
        phase,
        task: None,
        cancel: None,
        last_info,
        channel: None,
    }
}

/// A state that has already settled a check: it speaks for a channel. `state_holding`
/// leaves that unset, which is the first check of a session and adopts whatever it is given.
fn state_on(
    channel: UpdateChannel,
    phase: UpdaterPhase,
    last_info: Option<UpdateInfo>,
) -> UpdaterState {
    UpdaterState {
        channel: Some(channel),
        ..state_holding(phase, last_info)
    }
}

fn offer(version: &str) -> Option<UpdateInfo> {
    Some(UpdateInfo {
        version: version.to_string(),
        download_size: 42,
    })
}

/// A check owns the offer its answer disproves: up to date, or withheld by a gate that would
/// refuse the download further down. It keeps the offer when it proved nothing (a transient
/// failure) or while an operation acts on it: only a download undoes a download.
#[test]
fn a_check_retracts_only_the_offer_it_disproves() {
    for (what, phase, outcome, kept) in [
        (
            "up to date",
            UpdaterPhase::Idle,
            CheckOutcome::UpToDate,
            false,
        ),
        (
            "failed",
            UpdaterPhase::Idle,
            CheckOutcome::Failed {
                reason: "dns error".to_string(),
            },
            true,
        ),
        (
            "withheld",
            UpdaterPhase::Idle,
            CheckOutcome::Withheld {
                reason: "bootstrap behind".to_string(),
            },
            false,
        ),
        (
            "up to date during a download",
            UpdaterPhase::Downloading("9.9.9".to_string()),
            CheckOutcome::UpToDate,
            true,
        ),
        (
            "up to date while ready",
            UpdaterPhase::Ready("9.9.9".to_string()),
            CheckOutcome::UpToDate,
            true,
        ),
        (
            "up to date while applying",
            UpdaterPhase::Applying("9.9.9".to_string()),
            CheckOutcome::UpToDate,
            true,
        ),
    ] {
        let mut state = state_holding(phase, offer("9.9.9"));

        state.settle_check(&outcome, UpdateChannel::Stable);

        assert_eq!(
            state.last_info.as_ref().map(|i| i.version.as_str()),
            kept.then_some("9.9.9"),
            "{what}"
        );
    }
}

/// Apply spawns the updater child on the version it is handed. It compared that version to
/// the staged manifest only to decide whether to reuse the staged bytes, and never to refuse
/// the apply; an arbitrary version reached the child, which downloads it.
#[test]
fn an_apply_is_refused_for_a_version_the_state_never_staged() {
    let mut phase = UpdaterPhase::Ready("2.0.0".to_string());

    assert_eq!(
        phase.claim_apply("0.0.18-pre.dev.5"),
        ClaimVerdict::NotStaged,
        "the updater child would install a version nothing ever staged"
    );
    assert!(
        matches!(phase, UpdaterPhase::Ready(_)),
        "a refused apply must leave the staged update where it was"
    );
}

/// Refusing a version while it is being fetched means stop fetching it, and only it: left running,
/// the download completes and announces itself ready, handing back the very version that was
/// declined, with its bandwidth already spent.
#[test]
fn a_dismissal_stops_only_a_download_of_the_version_it_names() {
    for (dismissed, names_download) in [("9.9.9", true), ("1.0.0", false)] {
        let token = tokio_util::sync::CancellationToken::new();
        let mut state = UpdaterState {
            phase: UpdaterPhase::Downloading("9.9.9".to_string()),
            task: None,
            cancel: Some(token.clone()),
            last_info: offer("9.9.9"),
            channel: None,
        };

        let staging_must_go = state.dismiss_offer(dismissed);

        assert_eq!(
            token.is_cancelled(),
            names_download,
            "dismissing {dismissed}"
        );
        assert_eq!(staging_must_go, names_download, "dismissing {dismissed}");
        assert_eq!(
            matches!(state.phase, UpdaterPhase::Idle),
            names_download,
            "dismissing {dismissed}"
        );
    }
}

/// The end of a download announced `Ready` whatever had happened while it ran. A dismissal or
/// a channel change in that window left the state void, and the announcement handed the
/// surface a version it had already dropped.
#[test]
fn a_finished_download_nobody_wants_any_more_is_not_announced() {
    let mut state = state_holding(UpdaterPhase::Idle, None);

    assert!(
        !state.finish_download("9.9.9"),
        "a download the state had already voided announced itself ready"
    );
    assert!(matches!(state.phase, UpdaterPhase::Idle));
}

/// A staged update is an offer too: the phase names it and the status reply serves it, with
/// no `last_info` involved at all. Retracting only the record would leave the surface showing
/// "Ready to update" for the very version just declined. Boot already decided what a
/// dismissed staging deserves: it deletes it.
#[test]
fn a_dismissal_of_a_staged_version_asks_for_its_staging_to_go() {
    let mut state = state_holding(UpdaterPhase::Ready("9.9.9".to_string()), None);

    let staging_must_go = state.dismiss_offer("9.9.9");

    assert!(staging_must_go, "the staged copy would outlive the refusal");
    assert!(matches!(state.phase, UpdaterPhase::Idle));
}

/// An offer is derived from the channel it was found on. The setting can be changed with an
/// offer already on screen, and nothing re-checked or re-validated it: the dev build stayed
/// on offer, and downloading it installed a prerelease the switch to stable had just
/// declined.
#[test]
fn a_channel_change_retracts_the_offer_it_no_longer_speaks_for() {
    let mut state = state_holding(UpdaterPhase::Idle, offer("0.0.18-pre.dev.5"));

    assert!(!state.abandon_for_channel_change(UpdateChannel::Stable));

    assert!(state.last_info.is_none());
}

/// Choosing stable is not a way of asking for the dev build already on disk. Whichever step
/// that build had reached, it stops being installable, the staged copy included, which is
/// why the caller is told to delete it.
#[test]
fn a_channel_change_abandons_a_staged_build() {
    let mut state = state_holding(UpdaterPhase::Ready("0.0.18-pre.dev.5".to_string()), None);

    let staging_must_go = state.abandon_for_channel_change(UpdateChannel::Stable);

    assert!(staging_must_go);
    assert!(matches!(state.phase, UpdaterPhase::Idle));
}

/// A download in flight would otherwise finish, announce itself ready and offer the very
/// build the switch declined.
#[test]
fn a_channel_change_stops_a_download_it_no_longer_wants() {
    let token = tokio_util::sync::CancellationToken::new();
    let mut state = UpdaterState {
        phase: UpdaterPhase::Downloading("0.0.18-pre.dev.5".to_string()),
        task: None,
        cancel: Some(token.clone()),
        last_info: offer("0.0.18-pre.dev.5"),
        channel: Some(UpdateChannel::Dev),
    };

    let staging_must_go = state.abandon_for_channel_change(UpdateChannel::Stable);

    assert!(token.is_cancelled(), "the download would have completed");
    assert!(
        staging_must_go,
        "its partial staging would have been left behind"
    );
    assert!(matches!(state.phase, UpdaterPhase::Idle));
    assert!(state.last_info.is_none());
}

/// An apply has already spawned the updater child, which reads the staging directory to
/// install from. Deleting it under the child is worse than any channel it was chosen from.
#[test]
fn a_channel_change_spares_an_apply_already_spawning() {
    let mut state = state_holding(UpdaterPhase::Applying("0.0.18-pre.dev.5".to_string()), None);

    assert!(
        !state.abandon_for_channel_change(UpdateChannel::Stable),
        "the updater child would lose the files it is installing"
    );
    assert!(matches!(state.phase, UpdaterPhase::Applying(_)));
}

/// The exception above is for the child, not the offer, and a spawn that fails leaves no
/// child. Handing the claim back then puts the declined build back on offer through the
/// phase arm of `names_version`, which one click applies again, and the staging spared for
/// that child stays on disk naming a channel the user has left.
#[test]
fn a_claim_the_channel_left_behind_is_not_handed_back() {
    let mut state = state_on(
        UpdateChannel::Dev,
        UpdaterPhase::Ready("0.0.18-pre.dev.5".to_string()),
        offer("0.0.18-pre.dev.5"),
    );
    assert_eq!(
        state.phase.claim_apply("0.0.18-pre.dev.5"),
        ClaimVerdict::Claimed,
        "the staged version is on offer"
    );

    state.abandon_for_channel_change(UpdateChannel::Stable);
    let staging_must_go = state.release_apply("0.0.18-pre.dev.5", UpdateChannel::Dev);

    assert!(
        matches!(state.phase, UpdaterPhase::Idle),
        "the dev build came back on offer to a user who switched to stable"
    );
    assert!(
        staging_must_go,
        "the staging the switch spared for a child that never started outlives the switch"
    );
    assert!(!state.names_version("0.0.18-pre.dev.5"));
}

/// The common failure has no channel change in it, and wedging the phase in `Applying` for
/// the rest of the session is worse than the race the claim exists to stop: the staged
/// bytes are still good and the user can retry without fetching them again.
#[test]
fn a_claim_the_channel_still_speaks_for_comes_back() {
    let mut state = state_on(
        UpdateChannel::Dev,
        UpdaterPhase::Ready("0.0.18-pre.dev.5".to_string()),
        offer("0.0.18-pre.dev.5"),
    );
    assert_eq!(
        state.phase.claim_apply("0.0.18-pre.dev.5"),
        ClaimVerdict::Claimed,
        "the staged version is on offer"
    );

    let staging_must_go = state.release_apply("0.0.18-pre.dev.5", UpdateChannel::Dev);

    assert!(
        matches!(state.phase, UpdaterPhase::Ready(ref v) if v == "0.0.18-pre.dev.5"),
        "a failed spawn left the updater wedged in Applying for good"
    );
    assert!(
        !staging_must_go,
        "the staged bytes are still the right ones"
    );
    assert_eq!(
        state.phase.claim_apply("0.0.18-pre.dev.5"),
        ClaimVerdict::Claimed,
        "the retry the release exists to allow is refused"
    );
}

/// A staged build adopted at boot never settles a check; the state speaks for no channel
/// at all when the claim is taken. The premise has to come from the setting, which always
/// names one: read off the state it would be absent here, and absent compares equal to
/// everything the switch could move it to.
#[test]
fn a_claim_on_a_staged_build_still_answers_for_its_channel() {
    let mut state = state_holding(UpdaterPhase::Ready("0.0.18-pre.dev.5".to_string()), None);
    assert_eq!(
        state.phase.claim_apply("0.0.18-pre.dev.5"),
        ClaimVerdict::Claimed,
        "the staged version is on offer"
    );
    assert!(
        state.channel.is_none(),
        "a boot-adopted staging records no channel, which is what makes this case reachable"
    );

    state.abandon_for_channel_change(UpdateChannel::Stable);
    let staging_must_go = state.release_apply("0.0.18-pre.dev.5", UpdateChannel::Dev);

    assert!(
        matches!(state.phase, UpdaterPhase::Idle),
        "an absent premise read the switch as no switch at all"
    );
    assert!(staging_must_go);
}

/// A check that resolved under a channel since left is refused whole: its dev build would come
/// back as the standing offer, which `names_version` authorizes for download (the channel filter
/// lives in the check alone), and a barren answer would strip the offer the new channel holds.
#[test]
fn a_check_answering_for_a_channel_since_left_is_refused() {
    for (outcome, held) in [
        (
            CheckOutcome::Available {
                info: UpdateInfo {
                    version: "0.0.18-pre.dev.5".to_string(),
                    download_size: 42,
                },
            },
            None,
        ),
        (CheckOutcome::UpToDate, Some("1.0.0")),
    ] {
        let mut state = state_on(
            UpdateChannel::Stable,
            UpdaterPhase::Idle,
            held.and_then(offer),
        );

        let settlement = state.settle_check(&outcome, UpdateChannel::Dev);

        assert_eq!(settlement, CheckSettlement::Stale);
        assert_eq!(
            state.last_info.as_ref().map(|i| i.version.as_str()),
            held,
            "an answer for the abandoned channel changed the offer of the current one"
        );
    }
}

/// Nothing records the channel before the first check settles, and refusing there would
/// refuse every offer on a run where the user never touched the setting. That is an
/// establishment, not a switch.
#[test]
fn the_first_check_of_a_session_establishes_the_channel_it_answers_for() {
    let mut state = state_holding(UpdaterPhase::Idle, None);

    let settlement = state.settle_check(&CheckOutcome::UpToDate, UpdateChannel::Dev);

    assert_eq!(settlement, CheckSettlement::Settled);
    assert_eq!(
        state.channel,
        Some(UpdateChannel::Dev),
        "the state answers for no channel, so the next check cannot be placed"
    );
}

/// Compared by value, not counted. A user who flips to stable and back to dev while a dev
/// check is in flight gets an answer that is correct for where they now are, and a
/// generation counter (the idiom `LOAD_SEQ` follows one module over) would discard it for
/// having been overtaken rather than for being wrong.
#[test]
fn a_check_survives_a_channel_change_that_came_back_to_its_own() {
    let mut state = state_on(UpdateChannel::Dev, UpdaterPhase::Idle, None);

    state.abandon_for_channel_change(UpdateChannel::Stable);
    state.abandon_for_channel_change(UpdateChannel::Dev);
    let settlement = state.settle_check(
        &CheckOutcome::Available {
            info: UpdateInfo {
                version: "0.0.18-pre.dev.5".to_string(),
                download_size: 42,
            },
        },
        UpdateChannel::Dev,
    );

    assert_eq!(
        settlement,
        CheckSettlement::Settled,
        "an answer correct for the selected channel was thrown away"
    );
}

/// `updater.download` took the version from its caller and asked nothing else. A renderer
/// holding a record from before a channel switch (or any plugin, the channel being open to
/// every trusted frame) could name a version no check had offered, and the download path
/// fetches a release by exact tag with no channel filter of its own.
///
/// A version nothing offered is the one refusal that is not a phase: no surface may adopt a
/// state from it, and announcing one would paint a phase over a genuine error.
#[test]
fn a_download_of_a_version_nothing_offered_is_not_a_phase() {
    let state = state_holding(UpdaterPhase::Idle, offer("1.0.0"));

    assert_eq!(
        state.answer_download("0.0.18-pre.dev.5"),
        Some(DownloadRefusal::NotOnOffer)
    );
}

/// A previous session may have downloaded an update and never applied it, and a fresh process
/// must find it on disk. A dismissal outlives its session (the setting is persisted) and is keyed
/// on the version: a declined staging is deleted, another version's is still offered.
#[test]
fn a_staged_update_is_adopted_unless_the_user_declined_that_version() {
    for (version, skipped, verdict, phase) in [
        (
            "1.0.0",
            None,
            StagedVerdict::Adopted,
            UpdaterPhase::Ready("1.0.0".to_string()),
        ),
        (
            "1.0.0",
            Some("1.0.0"),
            StagedVerdict::Declined,
            UpdaterPhase::Idle,
        ),
        (
            "2.0.0",
            Some("1.0.0"),
            StagedVerdict::Adopted,
            UpdaterPhase::Ready("2.0.0".to_string()),
        ),
    ] {
        let mut state = state_holding(UpdaterPhase::Idle, None);

        assert_eq!(
            state.take_staged(version, skipped),
            verdict,
            "{version} with {skipped:?} declined"
        );
        assert_eq!(
            state.phase, phase,
            "a declined staging is deleted, not adopted and then dropped"
        );
    }
}

/// The scan reads the filesystem under no lock and runs on every login, not at boot alone. A
/// download writes its manifest before it takes the lock to claim `Ready`; the scan can
/// find a complete staging while the phase still reads `Downloading`, and the versions match
/// there, every time, because a download wipes the staging directory it starts from. Writing
/// `Ready` over that made the download's own `finish_download` refuse and delete the staging
/// this scan had just announced; deleting it as a dismissal took the bytes out from under a
/// download that then announced itself ready over nothing. An apply is worse: its updater
/// child installs from that directory.
#[test]
fn a_disk_scan_never_takes_a_phase_an_operation_holds() {
    for phase in [
        UpdaterPhase::Downloading("1.0.0".to_string()),
        UpdaterPhase::Ready("1.0.0".to_string()),
        UpdaterPhase::Applying("1.0.0".to_string()),
    ] {
        let mut state = state_holding(phase.clone(), None);

        assert_eq!(
            state.take_staged("1.0.0", None),
            StagedVerdict::Busy,
            "a disk scan overwrote {phase:?}"
        );
        assert_eq!(state.phase, phase, "{phase:?} was left changed");

        let mut dismissed = state_holding(phase.clone(), None);

        assert_eq!(
            dismissed.take_staged("1.0.0", Some("1.0.0")),
            StagedVerdict::Busy,
            "a dismissal deleted the staging {phase:?} is acting on"
        );
    }
}

/// Every phase, and the exact JSON the renderer's own union is written against.
///
/// The match below has no wildcard on purpose. A variant added to `UpdaterPhase` will not
/// compile here until someone names its wire tag, and that is the moment to add the matching
/// branch to `hydrateUpdater` in `frontend/plugins/ui/src/updater/state.ts`. Nothing else
/// links the two definitions: there is no codegen, the reply reaches the renderer untyped,
/// and a tag no branch there listed used to fall through in silence, which is how `Applying`
/// arrived at both surfaces painted as an offer.
#[test]
fn every_phase_names_itself_on_the_wire() {
    for phase in [
        UpdaterPhase::Idle,
        UpdaterPhase::Downloading("1.2.3".to_string()),
        UpdaterPhase::Ready("1.2.3".to_string()),
        UpdaterPhase::Applying("1.2.3".to_string()),
    ] {
        let expected = match &phase {
            UpdaterPhase::Idle => serde_json::json!({ "state": "Idle", "last_info": null }),
            UpdaterPhase::Downloading(v) => {
                serde_json::json!({ "state": "Downloading", "version": v, "last_info": null })
            }
            UpdaterPhase::Ready(v) => {
                serde_json::json!({ "state": "Ready", "version": v, "last_info": null })
            }
            UpdaterPhase::Applying(v) => {
                serde_json::json!({ "state": "Applying", "version": v, "last_info": null })
            }
        };

        let resp = StatusResponse {
            phase: &phase,
            last_info: &None,
        };

        assert_eq!(
            serde_json::to_value(&resp).unwrap(),
            expected,
            "the wire shape of {phase:?} changed"
        );
    }
}
