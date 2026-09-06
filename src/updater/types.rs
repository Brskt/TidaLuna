use std::collections::BTreeMap;
use std::sync::LazyLock;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex as TokioMutex;
use tokio_util::sync::CancellationToken;

use super::UpdateChannel;

// ---------------------------------------------------------------------------
// Types (shared with the updater crate via identical definitions)
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize)]
pub(super) struct Manifest {
    pub(super) version: String,
    pub(super) min_version: String,
    pub(super) target: String,
    pub(super) files: BTreeMap<String, FileEntry>,
    /// Linux-only: minimum value of `/usr/lib/tidalunar/SANDBOX_PROTOCOL_VERSION`
    /// the system bootstrap must have for this update to be safe to apply.
    /// Defaults to `None` for backwards compatibility with manifests generated
    /// before the field was added (2026-04). On non-Linux platforms the field
    /// is always omitted at packaging time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) sandbox_protocol_required: Option<u32>,
    /// Version this release's delta archive diffs against (the immediately
    /// previous release). `None` when no delta exists. Stamped by CI, not by
    /// `xtask bundle`. The updater downloads the delta only when this equals
    /// its own current version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) delta_from: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub(super) struct FileEntry {
    pub(super) sha256: String,
    pub(super) size: u64,
}

#[derive(Serialize, Deserialize)]
pub(super) struct Journal {
    pub(super) version: String,
    pub(super) state: String,
    pub(super) files: Vec<JournalFile>,
    #[serde(default)]
    pub(super) deleted_files: Vec<String>,
}

#[derive(Serialize, Deserialize)]
pub(super) struct JournalFile {
    pub(super) path: String,
    pub(super) backup: String,
    #[serde(default)]
    pub(super) is_new: bool,
}

#[derive(Deserialize)]
pub(super) struct GhRelease {
    pub(super) tag_name: String,
    pub(super) assets: Vec<GhAsset>,
    #[serde(default)]
    pub(super) prerelease: bool,
    #[serde(default)]
    pub(super) draft: bool,
}

#[derive(Deserialize)]
pub(super) struct GhAsset {
    pub(super) name: String,
    pub(super) browser_download_url: String,
    #[serde(default)]
    pub(super) size: u64,
}

/// Information about an available update, sent to the frontend.
#[derive(Serialize, Clone)]
pub(crate) struct UpdateInfo {
    pub version: String,
    pub download_size: u64,
}

// ---------------------------------------------------------------------------
// Updater state machine
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", content = "version")]
pub(crate) enum UpdaterPhase {
    Idle,
    Downloading(String),
    Ready(String),
    Applying(String),
}

/// The `updater.status` reply, as the renderer receives it.
///
/// Held beside the phase it carries rather than inside the handler, so a test can pin the shape
/// that crosses the bridge. The renderer reads these tags with a hand-written union
/// (`hydrateUpdater` in `frontend/plugins/ui/src/updater/state.ts`) and there is no codegen
/// between the two: a variant added here and left unlisted there stops matching, which is how
/// `Applying` reached both surfaces painted as an offer.
#[derive(Serialize)]
pub(super) struct StatusResponse<'a> {
    #[serde(flatten)]
    pub(super) phase: &'a UpdaterPhase,
    pub(super) last_info: &'a Option<UpdateInfo>,
}

/// What a claim on the apply slot concluded.
///
/// Named because one `None` answered two questions the handler settles differently: a version
/// nothing staged is an error the asker has to see, while an apply already in flight is a phase
/// to re-state. Telling them apart otherwise meant reading the phase in the handler, one line
/// before the claim looked at it itself.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum ClaimVerdict {
    /// The slot was free and this version is the staged one. The phase now names the apply.
    Claimed,
    /// An apply already holds the slot. The phase is the answer, as it is for the download
    /// this same phase refuses.
    InFlight,
    /// Nothing this state staged names that version: there is nothing to apply.
    NotStaged,
}

impl UpdaterPhase {
    /// Claim the phase for an apply of `version`.
    ///
    /// Only a staged version may be applied: the version travels from the caller straight to
    /// the updater child, which downloads whatever it is handed, so anything able to send the
    /// message could install a version nothing had staged. One apply at a time, since two claims
    /// race the staging directory and spawn two updater children for the same pid. Split from
    /// the handler, which needs an exe path, a process and a CEF task before this decision, to
    /// keep the rule testable.
    pub(super) fn claim_apply(&mut self, version: &str) -> ClaimVerdict {
        if matches!(self, UpdaterPhase::Applying(_)) {
            return ClaimVerdict::InFlight;
        }
        if !matches!(self, UpdaterPhase::Ready(v) if v == version) {
            return ClaimVerdict::NotStaged;
        }
        *self = UpdaterPhase::Applying(version.to_string());
        ClaimVerdict::Claimed
    }
}

/// What a check concluded, named rather than flattened into an `Option`.
///
/// Thirteen paths used to answer `None`: one meant "nothing to install", two "nothing you can
/// install", six were transient failures and the rest could not say. A caller handed that cannot
/// decide whether to take an offer back, and the two guessed differently: one kept a disproved
/// version on screen, the other said nothing at all.
#[derive(Clone, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub(crate) enum CheckOutcome {
    /// A newer version this install can take.
    Available { info: UpdateInfo },
    /// The release list was read and holds nothing newer this install can take.
    UpToDate,
    /// A newer version exists and a gate refuses it here. The reason is what the user can
    /// act on; the version itself is not an offer, because the gates further down would
    /// reject the download it would start.
    Withheld { reason: String },
    /// The check never reached an answer.
    Failed { reason: String },
}

/// Why a download is refused before any work starts, named for the refusal to carry the
/// phase it refused into.
///
/// Each used to be answered to the asker alone, as a reply string. A surface paints
/// `downloading` on the click, and only the asker ever learned that no download would start:
/// the toast reads no reply at all and kept that paint for the session, and the settings page
/// turned one refusal into the literal error text "applying".
#[derive(Debug, PartialEq, Eq)]
pub(super) enum DownloadRefusal {
    /// A version this state never put on the table. Nothing about the phase changed, and no
    /// surface may adopt one: this is an error, not a phase.
    NotOnOffer,
    /// A download is already running. The asker's own `downloading` is the truth here, and
    /// every other surface already reads it: this refusal announces nothing.
    InProgress,
    /// This exact version is already staged and needs no fetching.
    AlreadyReady,
    /// An apply is in flight. The phase is the answer, as it is for `updater.apply`'s own
    /// refusal of a second claim.
    Applying,
}

/// What became of a staged update a disk scan found.
///
/// Named because the scan's branch did the work itself and consulted nothing: it wrote `Ready`
/// over whatever phase was there, and its other half deleted the staging directory without
/// asking what was using it.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum StagedVerdict {
    /// Taken as this state's own offer. The caller announces it.
    Adopted,
    /// The user declined this very version, and nothing of this state's is at stake: the
    /// caller deletes the staging.
    Declined,
    /// An operation holds the phase. The staging belongs to it, and so does the phase.
    Busy,
}

/// Whether a check's answer still speaks for the channel this state is on.
///
/// Named because settlement used to be silent and unconditional. A check reads its channel
/// before a network round trip and settles after one, so an answer can arrive for a channel the
/// user has left, whose offer `abandon_for_channel_change` had already taken back. The write
/// landed anyway, restoring a dev release as the standing offer that `names_version` then
/// authorizes a download of: the channel filter lives in the check alone, so an offer is the
/// only thing keeping a channel's build out.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum CheckSettlement {
    /// Taken as this state's own answer. The caller announces it.
    Settled,
    /// Resolved under a channel the user has since left. Nothing of it is adopted: not the
    /// offer it found, and not the retraction a barren answer would imply, because it
    /// searched the wrong release list either way.
    Stale,
}

pub(crate) struct UpdaterState {
    pub phase: UpdaterPhase,
    pub task: Option<tokio::task::JoinHandle<()>>,
    pub cancel: Option<CancellationToken>,
    pub last_info: Option<UpdateInfo>,
    /// The channel this state's offer speaks for, or `None` before any check has settled.
    ///
    /// The premise every check resolves its answer from, held here because the answer arrives
    /// after the premise can have changed. Every other rule keys on a version string, and a
    /// version says nothing about which release list was searched to find it.
    pub channel: Option<UpdateChannel>,
}

impl UpdaterState {
    pub(super) fn reset_task(&mut self) {
        self.task = None;
        self.cancel = None;
    }

    pub(super) fn reset_to_idle(&mut self) {
        self.phase = UpdaterPhase::Idle;
        self.reset_task();
    }

    /// Settle the offer a check's answer implies: publish what it found, take back what it
    /// disproved, keep what it could not speak to, and refuse the lot when the answer was
    /// resolved under a channel this state has since left.
    ///
    /// Both check paths settle here. The offer had a writer and no retractor, and the manual
    /// check touched neither, so a record could outlive every check that ruled it out while
    /// `updater.status` went on serving it to each new mount.
    ///
    /// `resolved_under` is the channel the caller read before its round trip. Compared by value
    /// rather than counted, because the question is not whether the setting moved but whether
    /// this answer still answers for where we are: a channel flipped away and back leaves an
    /// answer that is correct again, which a generation counter would throw away.
    ///
    /// The first check of a session finds no channel recorded and establishes it; nothing else
    /// can have set it by then, since a change would have.
    pub(super) fn settle_check(
        &mut self,
        outcome: &CheckOutcome,
        resolved_under: UpdateChannel,
    ) -> CheckSettlement {
        if self.channel.is_some_and(|on| on != resolved_under) {
            return CheckSettlement::Stale;
        }
        self.channel = Some(resolved_under);
        match outcome {
            CheckOutcome::Available { info } => self.last_info = Some(info.clone()),
            CheckOutcome::UpToDate | CheckOutcome::Withheld { .. } => self.retract_offer(),
            // Nothing disproved means nothing taken back.
            CheckOutcome::Failed { .. } => {}
        }
        CheckSettlement::Settled
    }

    /// Take back the offer a check just disproved.
    ///
    /// A download, a staged update and an apply each act on the offer they were started
    /// with, and this answers for the release list rather than for that operation; anything
    /// in flight keeps its record.
    pub(super) fn retract_offer(&mut self) {
        if matches!(self.phase, UpdaterPhase::Idle) {
            self.last_info = None;
        }
    }

    /// Void what this state derived from the update channel, and say whether a staging
    /// directory has to be deleted with it.
    ///
    /// Choosing stable is not a way of asking for the dev build already found. That build stops
    /// being installable at whatever step it had reached: the offer goes, a download in flight
    /// is stopped before it can announce itself ready, and the staged copy is handed to the
    /// caller for deletion, a few hundred megabytes of filesystem work not belonging under this
    /// lock. An apply is the exception, and not for the offer's sake: the updater child is
    /// already running and installs *from* that staging directory.
    ///
    /// Recording `now` is the other half: taking back what the old channel produced settles
    /// nothing while a check resolved under it is still in flight, and this is the operation
    /// that knows the premise moved. `settle_check` reads what is written here to refuse that
    /// late answer.
    pub(super) fn abandon_for_channel_change(&mut self, now: UpdateChannel) -> bool {
        self.channel = Some(now);
        self.last_info = None;
        match &self.phase {
            UpdaterPhase::Downloading(_) => {
                self.stop_download();
                self.reset_to_idle();
                true
            }
            UpdaterPhase::Ready(_) => {
                self.reset_to_idle();
                true
            }
            UpdaterPhase::Applying(_) | UpdaterPhase::Idle => false,
        }
    }

    /// Release a claim no updater child ever started, and say whether a staging directory has
    /// to be deleted with it.
    ///
    /// `abandon_for_channel_change` spares an apply for the child's sake, not the offer's. A
    /// spawn that fails leaves no child, so that exception has nothing left to protect: the
    /// build the switch declined must not come back on offer with the phase, and the staging it
    /// was spared is owed the deletion the switch deferred.
    ///
    /// `claimed_under` is the channel the setting named when the claim was taken, compared
    /// against the one this state is on. `settle_check`'s shape for its reason, and by value
    /// rather than counted, letting a channel flipped away and back release a claim that is
    /// correct again.
    ///
    /// The version is reconstructed rather than restored from a captured phase, which cannot
    /// know what moved underneath it. Nothing but this claim moves the phase out of `Applying`:
    /// a phase that no longer names the version belongs to something else and keeps it.
    pub(super) fn release_apply(&mut self, version: &str, claimed_under: UpdateChannel) -> bool {
        if !matches!(&self.phase, UpdaterPhase::Applying(v) if v == version) {
            return false;
        }
        if self.channel.is_some_and(|on| on != claimed_under) {
            self.reset_to_idle();
            return true;
        }
        self.phase = UpdaterPhase::Ready(version.to_string());
        false
    }

    /// End everything a dismissal names, and say whether a staging directory has to go with it.
    ///
    /// A version is offered from three places: the record a check published, the phase a staged
    /// download left behind, and a download still running towards it. Persisting the skip
    /// setting left all three standing. Keyed on the version, so dismissing one update neither
    /// drops an offer for another nor stops a download of it; boot already deletes a staging
    /// whose version is dismissed, and this takes the same decision at the refusal.
    pub(super) fn dismiss_offer(&mut self, version: &str) -> bool {
        if self
            .last_info
            .as_ref()
            .is_some_and(|i| i.version == version)
        {
            self.last_info = None;
        }
        match &self.phase {
            UpdaterPhase::Downloading(v) if v == version => {
                self.stop_download();
                self.reset_to_idle();
                true
            }
            UpdaterPhase::Ready(v) if v == version => {
                self.reset_to_idle();
                true
            }
            _ => false,
        }
    }

    /// Hand a finished download the `Ready` phase, but only while this state is still waiting
    /// for that very download. `false` when something voided it in the meantime (a
    /// dismissal, a channel change), and the caller then owns bytes nobody asked for.
    ///
    /// The announcement was unconditional, and an operation already ended came back to offer
    /// itself anyway.
    pub(super) fn finish_download(&mut self, version: &str) -> bool {
        if !matches!(&self.phase, UpdaterPhase::Downloading(v) if v == version) {
            return false;
        }
        self.phase = UpdaterPhase::Ready(version.to_string());
        self.reset_task();
        true
    }

    /// What this state may do with a staged update a disk scan found, and doing it.
    ///
    /// The scan runs on every login, not only at boot, and reads the filesystem under no lock.
    /// Acting on what it finds while an operation holds the phase is what this refuses:
    ///
    /// - a download writes its manifest before it takes the lock to claim `Ready`, so a scan
    ///   can find a complete staging while the phase still reads `Downloading`. Adopting there
    ///   makes that download's `finish_download` refuse and delete the staging this scan
    ///   announced; deleting there takes the bytes from under a download that then announces
    ///   itself ready over nothing;
    /// - an apply's updater child installs *from* that directory, and the phase belongs to the
    ///   claim that spawned it: `release_apply` puts that claim back only while the phase still
    ///   names it, and a scan that replaced it meanwhile leaves a claim nobody releases;
    /// - a `Ready` is an answer this process already gave its surfaces, served to each new
    ///   mount by `updater.status`, and re-announcing it is not what a reload depends on.
    ///
    /// Only `Idle` holds nothing of its own, the same line `retract_offer` draws.
    pub(super) fn take_staged(&mut self, version: &str, skipped: Option<&str>) -> StagedVerdict {
        if !matches!(self.phase, UpdaterPhase::Idle) {
            return StagedVerdict::Busy;
        }
        // Keyed on the version, as every other dismissal is: declining one update says
        // nothing about the staging of another.
        if skipped == Some(version) {
            return StagedVerdict::Declined;
        }
        self.phase = UpdaterPhase::Ready(version.to_string());
        StagedVerdict::Adopted
    }

    /// Stop a download in flight without waiting on it: the token asks the task to bail at
    /// its next check, and the abort covers one already past its last.
    fn stop_download(&mut self) {
        if let Some(token) = self.cancel.take() {
            token.cancel();
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }

    /// Whether `version` is one this state itself put on the table: the offer a check
    /// published, or the staged update a boot detected and announced.
    ///
    /// Nothing else may be acted on. The download path fetches a release by exact tag and
    /// carries no channel filter of its own, so an operation trusting a version from its caller
    /// installs from a channel the user has left, and `updater.*` is open to every trusted
    /// frame rather than to the settings page alone.
    pub(super) fn names_version(&self, version: &str) -> bool {
        if self
            .last_info
            .as_ref()
            .is_some_and(|i| i.version == version)
        {
            return true;
        }
        matches!(
            &self.phase,
            UpdaterPhase::Downloading(v) | UpdaterPhase::Ready(v) | UpdaterPhase::Applying(v)
                if v == version
        )
    }

    /// What a download of `version` resolves to before any work starts. `None` when nothing
    /// holds it back and the fetch may begin.
    ///
    /// Named because a refusal has to say which phase it refused into, not merely that it
    /// refused: see [`DownloadRefusal`]. Split from the handler like `claim_apply`, leaving the
    /// rule testable and the handler around it not.
    pub(super) fn answer_download(&self, version: &str) -> Option<DownloadRefusal> {
        if !self.names_version(version) {
            return Some(DownloadRefusal::NotOnOffer);
        }
        match &self.phase {
            UpdaterPhase::Downloading(_) => Some(DownloadRefusal::InProgress),
            UpdaterPhase::Ready(v) if v == version => Some(DownloadRefusal::AlreadyReady),
            UpdaterPhase::Applying(_) => Some(DownloadRefusal::Applying),
            // A `Ready` naming some other version is staging this download replaces, which
            // the caller clears before it starts.
            _ => None,
        }
    }
}

pub(crate) static UPDATER_STATE: LazyLock<TokioMutex<UpdaterState>> = LazyLock::new(|| {
    TokioMutex::new(UpdaterState {
        phase: UpdaterPhase::Idle,
        task: None,
        cancel: None,
        last_info: None,
        channel: None,
    })
});

impl Manifest {
    pub(super) fn verify_target(&self) -> Result<(), anyhow::Error> {
        if self.target != super::TARGET {
            anyhow::bail!(
                "manifest target mismatch: expected {}, got {}",
                super::TARGET,
                self.target
            );
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "../../tests/unit/updater/types.rs"]
mod tests;
