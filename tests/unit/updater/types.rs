//! Tests for `src/updater/types.rs`, attached to it by `#[path]`.

use super::*;

#[test]
fn manifest_roundtrip_with_protocol_field() {
    let json = r#"{
        "version": "0.0.5-alpha",
        "min_version": "0.0.4-alpha",
        "target": "linux-amd64",
        "files": {},
        "sandbox_protocol_required": 1
    }"#;
    let m: Manifest = serde_json::from_str(json).unwrap();
    assert_eq!(m.sandbox_protocol_required, Some(1));
    let serialized = serde_json::to_string(&m).unwrap();
    assert!(serialized.contains("\"sandbox_protocol_required\":1"));
}

#[test]
fn manifest_roundtrip_without_protocol_field_defaults_none() {
    // Older manifests (pre-2026-04) did not have this field.
    let json = r#"{
        "version": "0.0.4-alpha",
        "min_version": "0.0.4-alpha",
        "target": "windows-amd64",
        "files": {}
    }"#;
    let m: Manifest = serde_json::from_str(json).unwrap();
    assert_eq!(m.sandbox_protocol_required, None);
}

#[test]
fn manifest_roundtrip_with_delta_from() {
    let json = r#"{
        "version": "0.0.5-alpha",
        "min_version": "0.0.4-alpha",
        "target": "linux-amd64",
        "files": {},
        "delta_from": "0.0.4-alpha"
    }"#;
    let m: Manifest = serde_json::from_str(json).unwrap();
    assert_eq!(m.delta_from.as_deref(), Some("0.0.4-alpha"));
    let s = serde_json::to_string(&m).unwrap();
    assert!(s.contains("\"delta_from\":\"0.0.4-alpha\""));
}

#[test]
fn manifest_without_delta_from_defaults_none() {
    let json = r#"{"version":"0.0.4-alpha","min_version":"0.0.4-alpha","target":"linux-amd64","files":{}}"#;
    let m: Manifest = serde_json::from_str(json).unwrap();
    assert_eq!(m.delta_from, None);
}

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

/// A check that reaches an answer owns the offer that answer contradicts. While the check
/// replied with a bare `Option`, publishing an offer had a writer and retracting one had
/// none: the record outlived every check that disproved it. The settings page then had no
/// reachable state left in which to say "You're up to date", and both surfaces kept a
/// Download button pointed at a version the check had just ruled out.
#[test]
fn a_check_that_finds_nothing_retracts_the_offer_it_disproves() {
    let mut state = state_holding(UpdaterPhase::Idle, offer("9.9.9"));

    state.settle_check(&CheckOutcome::UpToDate, UpdateChannel::Stable);

    assert!(
        state.last_info.is_none(),
        "the surfaces go on offering a version the check ruled out"
    );
}

/// Only one of the paths that answered `None` meant "there is nothing to install". Six were
/// transient (a dropped connection, DNS, a manifest that would not parse), and retracting
/// on those would tell a user whose update is genuinely waiting that they are current. The
/// offer stands until a check actually disproves it.
#[test]
fn a_check_that_could_not_conclude_keeps_the_offer() {
    let mut state = state_holding(UpdaterPhase::Idle, offer("9.9.9"));

    state.settle_check(
        &CheckOutcome::Failed {
            reason: "dns error".to_string(),
        },
        UpdateChannel::Stable,
    );

    assert_eq!(
        state.last_info.as_ref().map(|i| i.version.as_str()),
        Some("9.9.9"),
        "a flaky network is not evidence that an update vanished"
    );
}

/// An update this install may not take is not an offer either. The floor on `min_version`
/// and the Linux sandbox-protocol gate both find a real newer version and both refuse it,
/// and a Download button wired to one hands `updater.download` a version the gates reject
/// further down. What the user can act on is the reason, which travels on its own.
#[test]
fn an_update_this_install_may_not_take_is_retracted_too() {
    let mut state = state_holding(UpdaterPhase::Idle, offer("9.9.9"));

    state.settle_check(
        &CheckOutcome::Withheld {
            reason: "bootstrap behind".to_string(),
        },
        UpdateChannel::Stable,
    );

    assert!(
        state.last_info.is_none(),
        "an update the gates would refuse is still being offered"
    );
}

/// A check can land while a download or an apply is already acting on the offer, because
/// nothing stops one being started from the settings page mid-flight. Retracting then would
/// pull the record out from under the operation, which is the rule the renderer's cancel
/// already follows: only a download undoes a download.
#[test]
fn a_retraction_spares_an_offer_an_operation_is_acting_on() {
    for phase in [
        UpdaterPhase::Downloading("9.9.9".to_string()),
        UpdaterPhase::Ready("9.9.9".to_string()),
        UpdaterPhase::Applying("9.9.9".to_string()),
    ] {
        let mut state = state_holding(phase.clone(), offer("9.9.9"));

        state.settle_check(&CheckOutcome::UpToDate, UpdateChannel::Stable);

        assert!(
            state.last_info.is_some(),
            "a stale check stripped the record from under {phase:?}"
        );
    }
}

/// `updater.download` took the version from its caller and asked nothing else. A renderer
/// holding a record from before a channel switch (or any plugin, the channel being open to
/// every trusted frame) could name a version no check had offered, and the download path
/// fetches a release by exact tag with no channel filter of its own.
#[test]
fn a_download_is_refused_for_a_version_no_check_ever_offered() {
    let state = state_holding(UpdaterPhase::Idle, offer("1.0.0"));

    assert!(
        !state.names_version("0.0.18-pre.dev.5"),
        "a caller's own version is treated as an offer"
    );
}

#[test]
fn a_download_takes_the_offer_a_check_published() {
    let state = state_holding(UpdaterPhase::Idle, offer("1.0.0"));

    assert!(state.names_version("1.0.0"));
}

/// The offer has a second legitimate producer: a boot that finds a staged update sets the
/// phase to `Ready(v)` and announces it without ever publishing `last_info`. A guard that
/// asked `last_info` alone would refuse the apply that startup path exists to offer.
#[test]
fn a_download_takes_the_version_a_boot_found_staged() {
    let state = state_holding(UpdaterPhase::Ready("2.0.0".to_string()), None);

    assert!(
        state.names_version("2.0.0"),
        "the staged update a boot detected is an offer the state made itself"
    );
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

/// Dismissing a version persisted a setting and stopped there. The offer it named stayed in
/// memory, `updater.status` kept serving it to every new mount, and the toast's own
/// suppression is per-mount; the version the user had declined came back on screen the
/// next time the module was evaluated.
#[test]
fn a_dismissal_takes_back_the_offer_it_names() {
    let mut state = state_holding(UpdaterPhase::Idle, offer("9.9.9"));

    let staging_must_go = state.dismiss_offer("9.9.9");

    assert!(state.last_info.is_none());
    assert!(!staging_must_go, "nothing was staged to delete");
}

/// Refusing a version while it is being fetched means stop fetching it. Left running, the
/// download completes and announces itself ready, handing back the very version that was
/// declined, with its bandwidth already spent.
#[test]
fn a_dismissal_stops_a_download_of_the_version_it_names() {
    let token = tokio_util::sync::CancellationToken::new();
    let mut state = UpdaterState {
        phase: UpdaterPhase::Downloading("9.9.9".to_string()),
        task: None,
        cancel: Some(token.clone()),
        last_info: offer("9.9.9"),
        channel: None,
    };

    let staging_must_go = state.dismiss_offer("9.9.9");

    assert!(token.is_cancelled(), "it would have finished downloading");
    assert!(staging_must_go, "its partial staging would have been left");
    assert!(matches!(state.phase, UpdaterPhase::Idle));
}

#[test]
fn a_dismissal_leaves_a_download_of_another_version_alone() {
    let token = tokio_util::sync::CancellationToken::new();
    let mut state = UpdaterState {
        phase: UpdaterPhase::Downloading("9.9.9".to_string()),
        task: None,
        cancel: Some(token.clone()),
        last_info: offer("9.9.9"),
        channel: None,
    };

    state.dismiss_offer("1.0.0");

    assert!(
        !token.is_cancelled(),
        "declining one version stopped the download of another"
    );
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

#[test]
fn a_finished_download_the_state_still_waits_for_becomes_ready() {
    let mut state = state_holding(
        UpdaterPhase::Downloading("9.9.9".to_string()),
        offer("9.9.9"),
    );

    assert!(state.finish_download("9.9.9"));
    assert!(matches!(&state.phase, UpdaterPhase::Ready(v) if v == "9.9.9"));
}

/// A dismissal names one version. Taking back an offer for a different one would drop an
/// update the user never declined.
#[test]
fn a_dismissal_leaves_an_offer_for_another_version() {
    let mut state = state_holding(UpdaterPhase::Idle, offer("9.9.9"));

    state.dismiss_offer("1.0.0");

    assert_eq!(
        state.last_info.as_ref().map(|i| i.version.as_str()),
        Some("9.9.9")
    );
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

/// A check settling while the claim is held records the channel it resolved under, and the
/// first check of a session finds none recorded. That is an establishment, not a switch:
/// reading the premise off the state would refuse a claim nobody contradicted.
#[test]
fn a_first_check_settling_under_the_same_channel_releases_the_claim() {
    let mut state = state_holding(
        UpdaterPhase::Ready("0.0.18-pre.dev.5".to_string()),
        offer("0.0.18-pre.dev.5"),
    );
    assert_eq!(
        state.phase.claim_apply("0.0.18-pre.dev.5"),
        ClaimVerdict::Claimed,
        "the staged version is on offer"
    );

    state.settle_check(
        &CheckOutcome::Available {
            info: UpdateInfo {
                version: "0.0.18-pre.dev.5".to_string(),
                download_size: 42,
            },
        },
        UpdateChannel::Dev,
    );
    let staging_must_go = state.release_apply("0.0.18-pre.dev.5", UpdateChannel::Dev);

    assert!(matches!(state.phase, UpdaterPhase::Ready(_)));
    assert!(!staging_must_go);
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

/// Taking the old channel's offer back settled nothing while a check resolved under it was
/// still awaiting the network. That check searched the dev release list, and it committed its
/// answer after the retraction had run: the dev build came back as the standing offer, and
/// `names_version` authorizes a download of whatever the offer names: the channel filter
/// lives in the check alone, and the offer is the only thing keeping a channel's build out.
#[test]
fn a_check_answering_for_a_channel_since_left_is_refused() {
    let mut state = state_on(UpdateChannel::Stable, UpdaterPhase::Idle, None);

    let settlement = state.settle_check(
        &CheckOutcome::Available {
            info: UpdateInfo {
                version: "0.0.18-pre.dev.5".to_string(),
                download_size: 42,
            },
        },
        UpdateChannel::Dev,
    );

    assert_eq!(settlement, CheckSettlement::Stale);
    assert!(
        state.last_info.is_none(),
        "a prerelease is on offer after the switch to stable, and downloadable with it"
    );
}

/// A refusal has to cover the retracting half too. A barren answer from the channel just
/// left says nothing about the one now selected, and letting it through would strip a fresh
/// offer the new channel legitimately holds, the same defect by the other end.
#[test]
fn a_stale_check_may_not_retract_the_offer_the_new_channel_holds() {
    let mut state = state_on(UpdateChannel::Stable, UpdaterPhase::Idle, offer("1.0.0"));

    let settlement = state.settle_check(&CheckOutcome::UpToDate, UpdateChannel::Dev);

    assert_eq!(settlement, CheckSettlement::Stale);
    assert_eq!(
        state.last_info.as_ref().map(|i| i.version.as_str()),
        Some("1.0.0"),
        "an answer for the abandoned channel took back the offer of the current one"
    );
}

/// The guard refuses an answer for another channel and nothing else: a check on the channel
/// the state is on settles exactly as before.
#[test]
fn a_check_on_the_channel_the_state_is_on_settles() {
    let mut state = state_on(UpdateChannel::Dev, UpdaterPhase::Idle, None);

    let settlement = state.settle_check(
        &CheckOutcome::Available {
            info: UpdateInfo {
                version: "0.0.18-pre.dev.5".to_string(),
                download_size: 42,
            },
        },
        UpdateChannel::Dev,
    );

    assert_eq!(settlement, CheckSettlement::Settled);
    assert_eq!(
        state.last_info.as_ref().map(|i| i.version.as_str()),
        Some("0.0.18-pre.dev.5")
    );
}

/// Nothing records the channel before the first check settles, and refusing there would
/// refuse every offer on a run where the user never touched the setting.
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

/// The change is what knows the premise moved; it is what records it. Without this the
/// guard above has nothing to compare against and every late answer settles.
#[test]
fn a_channel_change_records_the_channel_it_leaves_the_state_on() {
    let mut state = state_on(
        UpdateChannel::Dev,
        UpdaterPhase::Idle,
        offer("0.0.18-pre.dev.5"),
    );

    state.abandon_for_channel_change(UpdateChannel::Stable);

    assert_eq!(state.channel, Some(UpdateChannel::Stable));
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

/// The publishing half belongs to the same owner. Two callers wrote the offer by hand (one
/// of them not at all), which is how the manual check came to refresh neither the record it
/// serves on the next mount nor the stale one it had just disproved.
#[test]
fn a_check_that_finds_an_update_publishes_it_as_the_offer() {
    let mut state = state_holding(UpdaterPhase::Idle, None);

    state.settle_check(
        &CheckOutcome::Available {
            info: UpdateInfo {
                version: "9.9.9".to_string(),
                download_size: 42,
            },
        },
        UpdateChannel::Stable,
    );

    assert_eq!(
        state.last_info.as_ref().map(|i| i.version.as_str()),
        Some("9.9.9")
    );
}

/// The download refusals answered the asker and nobody else. A surface paints `downloading`
/// on the click, and the toast reads no reply at all; it kept that paint, with a Cancel
/// button the backend ignores, for an update already staged. Naming the refusal is what lets
/// the handler announce the phase instead of merely reporting that it refused.
#[test]
fn a_download_of_the_version_already_staged_is_refused_by_name() {
    let state = state_holding(UpdaterPhase::Ready("1.0.0".to_string()), offer("1.0.0"));

    assert_eq!(
        state.answer_download("1.0.0"),
        Some(DownloadRefusal::AlreadyReady),
        "a refusal that cannot name its phase cannot announce one"
    );
}

/// Two operations hold the phase, and a surface must not be told the same thing about both:
/// a running download makes the asker's own optimistic paint true, while an apply is a phase
/// it never painted.
#[test]
fn a_download_names_which_operation_holds_the_phase() {
    let downloading = state_holding(
        UpdaterPhase::Downloading("1.0.0".to_string()),
        offer("1.0.0"),
    );
    let applying = state_holding(UpdaterPhase::Applying("1.0.0".to_string()), offer("1.0.0"));

    assert_eq!(
        downloading.answer_download("1.0.0"),
        Some(DownloadRefusal::InProgress)
    );
    assert_eq!(
        applying.answer_download("1.0.0"),
        Some(DownloadRefusal::Applying)
    );
}

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

/// Staging for another version is what this download replaces, not a reason to refuse it:
/// the caller clears it before starting. Refusing here would strand a user on the older
/// staged build with no way to fetch the newer offer.
#[test]
fn a_download_replacing_another_staged_version_is_not_refused() {
    let state = state_holding(UpdaterPhase::Ready("1.0.0".to_string()), offer("2.0.0"));

    assert_eq!(
        state.answer_download("2.0.0"),
        None,
        "the offer a check published cannot be fetched while older staging sits there"
    );
}

#[test]
fn nothing_holds_back_a_download_of_the_offer_on_the_table() {
    let state = state_holding(UpdaterPhase::Idle, offer("1.0.0"));

    assert_eq!(state.answer_download("1.0.0"), None);
}

/// The reason the scan exists: a previous session downloaded an update and never applied it,
/// and a fresh process has to find it on disk because nothing in memory remembers it.
#[test]
fn a_staged_update_a_previous_session_left_becomes_the_offer() {
    let mut state = state_holding(UpdaterPhase::Idle, None);

    assert_eq!(state.take_staged("1.0.0", None), StagedVerdict::Adopted);
    assert_eq!(state.phase, UpdaterPhase::Ready("1.0.0".to_string()));
}

/// A dismissal outlives the session that made it (the setting is persisted and nothing ever
/// clears it), and the scan has to answer for a version the user already declined.
#[test]
fn a_staged_update_the_user_declined_is_deleted_rather_than_offered() {
    let mut state = state_holding(UpdaterPhase::Idle, None);

    assert_eq!(
        state.take_staged("1.0.0", Some("1.0.0")),
        StagedVerdict::Declined
    );
    assert_eq!(
        state.phase,
        UpdaterPhase::Idle,
        "a declined staging is deleted, not adopted and then dropped"
    );
}

#[test]
fn a_dismissal_of_another_version_leaves_this_staging_offerable() {
    let mut state = state_holding(UpdaterPhase::Idle, None);

    assert_eq!(
        state.take_staged("2.0.0", Some("1.0.0")),
        StagedVerdict::Adopted
    );
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

/// The offer travels beside the phase, not inside it.
///
/// The renderer needs both: the phase names the version an operation acts on, and the offer
/// carries the download size no tag can. Reading the version off the offer instead is what a
/// check landing mid-download would poison: the two must arrive as separate fields.
#[test]
fn the_offer_rides_beside_the_phase() {
    let offer = Some(UpdateInfo {
        version: "3.0.0".to_string(),
        download_size: 512,
    });

    let resp = StatusResponse {
        phase: &UpdaterPhase::Downloading("2.0.0".to_string()),
        last_info: &offer,
    };

    assert_eq!(
        serde_json::to_value(&resp).unwrap(),
        serde_json::json!({
            "state": "Downloading",
            "version": "2.0.0",
            "last_info": { "version": "3.0.0", "download_size": 512 },
        })
    );
}
