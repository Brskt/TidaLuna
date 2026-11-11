pub(crate) mod asio;
pub(crate) mod buffer;
pub(crate) mod cache;
pub(crate) mod crossfade;
pub(crate) mod dash;
mod declick;
// The decode loop the two Windows engines share: compiled under test on every host, its
// fake-sink tests needing no device.
#[cfg(any(target_os = "windows", test))]
mod decode_feed;
pub(crate) mod ipc;
mod length;
// The DASH fixture also builds the downloads' remux input.
#[cfg(test)]
pub(crate) use length::dash_fixture;
mod load;
mod load_state;
// Compiled where it serves and where it is tested: only the two Windows engines consult it,
// but the classification is pure and its tests earn their place on every host.
#[cfg(any(target_os = "windows", test))]
mod packet_failure;
mod protocol;
mod resume;
mod thread;
// Compiled under test too: the shared decode loop's tests run it on every host.
#[cfg(any(target_os = "windows", test))]
mod throttle;
mod track_identity;
mod volume;
#[cfg(target_os = "windows")]
pub(crate) mod wasapi;

use crate::audio::bandwidth::GovernorHandle;
use load_state::mint_if;
#[cfg(target_os = "windows")]
use std::sync::atomic::AtomicU32;
use std::sync::mpsc;
use tokio_util::sync::CancellationToken;
use volume::LevelStore;

use load::{CacheReadError, LoadRequest, ResumePolicy, read_cache_entry};
pub(crate) use load::{SegmentUrls, cdn_cache_status, log_response_headers};
pub(crate) use load_state::{
    Continuation, LoadOrigin, StopScope, begin_load, continue_load, current_gen, current_load,
    next_event_seq,
};
use protocol::PlayerCommand;
pub(crate) use protocol::{
    AudioDevice, DeviceErrorKind, MediaErrorCode, MediaFormatSnapshot, OutputMode, PlaybackState,
    PlayerEvent,
};
pub(crate) use track_identity::{
    canonical_track_id, refresh_retained_credential, refreshed_fetch_url,
};
pub(crate) use volume::{LevelReport, OutputLevel, Volume};

#[cfg(target_os = "windows")]
static EXCLUSIVE_STREAM_SEQ: AtomicU32 = AtomicU32::new(0);
#[cfg(target_os = "windows")]
static ASIO_STREAM_SEQ: AtomicU32 = AtomicU32::new(0);

pub struct Player {
    cmd_tx: mpsc::Sender<PlayerCommand>,
    rt_handle: tokio::runtime::Handle,
    load_handle: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Current download's cancellation token; cancelled on a new load or stop.
    download_cancel: std::sync::Mutex<Option<CancellationToken>>,
    /// The track currently committed in the pipeline, as `(canonical_id, format)`,
    /// or `None` when nothing is loaded. The player thread sets it in `handle_load`
    /// (commit time, alongside `current_track_id`) and clears it on every track-end
    /// path; `load` reads it on the IPC thread to resume a same-track re-assert
    /// instead of rebuilding. It tracks the *committed* track, not the *requested*
    /// one; a duplicate load racing a still-in-flight load of a different track
    /// cannot falsely match and resume a stale pipeline. stop() is pause-retain, and
    /// this survives a stop and the following play() resumes it in place.
    committed_track: std::sync::Arc<std::sync::Mutex<Option<(String, String)>>>,
    /// Every level the output plays, recorded in the order it was set. An unmute restores from
    /// here, because this handle is the one place all writers pass through: the frontend's
    /// `player.volume`, a Connect receiver, and the OS mixer's reports.
    output_level: std::sync::Mutex<OutputLevel>,
    /// `None` without a settings database, as in unit tests: the level then lasts one launch.
    level_store: Option<LevelStore>,
    /// The one governor arbitrating this player's link between its playback and its preload.
    governor: GovernorHandle,
}

pub(crate) use crate::util::fmt::{format_bytes, format_ms, short_id};

// Gated: its only caller outside `player` is Windows-only, and clippy denies an unused re-export.
#[cfg(target_os = "windows")]
pub(crate) use thread::output::is_default_selector;

impl Player {
    pub fn new<F>(callback: F, rt_handle: tokio::runtime::Handle) -> Self
    where
        F: Fn(PlayerEvent) + Send + 'static,
    {
        // `DB.get()` rather than `db()`, whose accessor panics when no actor was installed, which
        // is every unit test that builds a player.
        Self::with_settings(callback, rt_handle, crate::state::DB.get().cloned())
    }

    pub(crate) fn with_settings<F>(
        callback: F,
        rt_handle: tokio::runtime::Handle,
        settings: Option<crate::db::DbActor>,
    ) -> Self
    where
        F: Fn(PlayerEvent) + Send + 'static,
    {
        let (cmd_tx, cmd_rx) = mpsc::channel::<PlayerCommand>();

        // The fallback is not invented: `load_volume_sync` answers `true` for a settings row that
        // does not exist yet, so a missing actor and a fresh install agree. Same trade
        // `PlayerThread::new` makes for `crossfade_secs`.
        #[cfg(target_os = "windows")]
        let volume_sync_enabled = settings
            .as_ref()
            .is_none_or(|db| db.call_settings(crate::settings::load_volume_sync));
        #[cfg(not(target_os = "windows"))]
        let volume_sync_enabled = true;

        let restored = settings
            .as_ref()
            .and_then(|db| db.call_settings(crate::settings::load_output_level))
            .map(|(volume, audible)| {
                OutputLevel::restored(Volume::from_percent(volume), Volume::from_percent(audible))
            });

        let committed_track = std::sync::Arc::new(std::sync::Mutex::new(None::<(String, String)>));
        let thread_committed = committed_track.clone();
        let thread_level = restored.map(OutputLevel::volume);
        // On the runtime this player spawns its loads on, where the governor's task runs.
        let governor = {
            let _entered = rt_handle.enter();
            crate::audio::bandwidth::spawn_governor()
        };
        let thread_governor = governor.clone();
        std::thread::spawn(move || {
            if let Some(mut pt) = thread::PlayerThread::new(
                cmd_rx,
                callback,
                volume_sync_enabled,
                thread_level,
                thread_committed,
                thread_governor,
            ) {
                pt.run();
            }
        });

        Self {
            cmd_tx,
            rt_handle,
            load_handle: std::sync::Mutex::new(None),
            download_cancel: std::sync::Mutex::new(None),
            committed_track,
            output_level: std::sync::Mutex::new(restored.unwrap_or(OutputLevel::START)),
            level_store: settings.map(|db| LevelStore {
                db,
                unsaved: std::sync::Arc::new(std::sync::Mutex::new(None)),
            }),
            governor,
        }
    }

    /// The governor a preload staged for this player shares with its playback.
    pub fn governor(&self) -> &GovernorHandle {
        &self.governor
    }

    fn send_cmd(&self, cmd: PlayerCommand) -> anyhow::Result<()> {
        self.cmd_tx
            .send(cmd)
            .map_err(|_| anyhow::anyhow!("Player thread is dead"))
    }

    pub fn play(&self) -> anyhow::Result<()> {
        self.send_cmd(PlayerCommand::Play)
    }

    pub fn pause(&self) -> anyhow::Result<()> {
        self.send_cmd(PlayerCommand::Pause)
    }

    /// `scope` for the same reason the loads carry an origin: this bumps the generation too, and
    /// a preload staged after a controller stopped the speaker answers that controller's session.
    /// A scope that does not cover the music playing stops nothing.
    pub fn stop(&self, scope: StopScope) -> anyhow::Result<()> {
        // Pause-retain, not teardown: the in-flight load task and its download are
        // kept (the download rides its own download_cancel token, not the load
        // generation); a same-track re-assert resumes in place. The mint invalidates any
        // still-pending auto-play Load/ReplayRequest (rejected by handle_load's
        // stale-gate and the `continue_load` mint): playback cannot start after the stop;
        // it doesn't truncate the retained download, and a steady-state re-assert
        // resumes via Player::load, which bypasses both gates.
        if mint_if(|_, owner| scope.claim(owner)).is_none() {
            crate::vprintln!("[PLAYER] {scope:?} stop: not this scope's music, left playing");
            return Ok(());
        }
        let event_seq = next_event_seq();
        self.send_cmd(PlayerCommand::Stop(event_seq))?;
        if scope.orphans() {
            self.send_cmd(PlayerCommand::Orphan)?;
        }
        Ok(())
    }

    pub fn seek(&self, time: f64) -> anyhow::Result<()> {
        self.send_cmd(PlayerCommand::Seek(time))
    }

    #[cfg(target_os = "windows")]
    pub fn set_volume_sync(&self, enabled: bool) -> anyhow::Result<()> {
        self.send_cmd(PlayerCommand::SetVolumeSync(enabled))
    }

    pub fn set_crossfade_secs(&self, secs: u8) -> anyhow::Result<()> {
        self.send_cmd(PlayerCommand::SetCrossfadeSecs(secs))
    }

    pub fn get_audio_devices(&self, req_id: Option<String>) -> anyhow::Result<()> {
        self.send_cmd(PlayerCommand::GetAudioDevices(req_id))
    }

    pub fn set_audio_device(&self, device_id: String, mode: OutputMode) -> anyhow::Result<()> {
        self.send_cmd(PlayerCommand::SetAudioDevice {
            id: device_id,
            mode,
        })
    }
}

#[cfg(test)]
#[path = "../../tests/unit/player/fixture.rs"]
mod fixture;

#[cfg(test)]
#[path = "../../tests/unit/player/heap_probe.rs"]
pub(crate) mod heap_probe;
