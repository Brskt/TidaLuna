use crate::player::{Player, PlayerCommand};

/// A volume percentage, always finite and within `0.0..=100.0`. Same reasoning as
/// [`OutputMode`](super::OutputMode): the invariant lives in the type, not in whichever function
/// happens to be its only consumer today.
///
/// Not cosmetic: `player.volume` takes any JSON number and the frontend forwards Redux's value
/// unclamped, so an input whose `/ 100.0` quotient passes `f32::MAX` casts to `f32::INFINITY`,
/// which the cpal callback multiplies into every sample. A non-finite input sanitizes to
/// silence, never to full scale, the wrong guess here having to be the quiet one.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Volume(f64);

impl Volume {
    pub fn from_percent(v: f64) -> Self {
        Self(if v.is_finite() {
            v.clamp(0.0, 100.0)
        } else {
            0.0
        })
    }

    pub fn as_percent(self) -> f64 {
        self.0
    }

    /// The multiplier the output applies, within `0.0..=1.0`.
    pub fn as_gain(self) -> f32 {
        (self.0 / 100.0) as f32
    }

    fn is_silent(self) -> bool {
        self.0 <= 0.0
    }
}

/// The level the output plays at, and the one an unmute gives back.
///
/// Muted is a level of zero and nothing else: a Connect `setMute` carries no level, and an
/// unmute restores the last audible one, whoever set it. No mute flag can go stale behind it.
///
/// One level for the install, whichever TIDAL account a page loads: once anyone chose it
/// (`chosen`), the page takes it instead of TIDAL's per-account restore. A page level decided
/// before the page took this one in is refused ([`Self::admits`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OutputLevel {
    volume: Volume,
    audible: Volume,
    /// Bumped by every change, whoever made it, and by every page catching up with the level.
    seq: u64,
    /// `seq` of the last change no page made (a Connect controller, the OS mixer, an earlier
    /// launch) or of the last page catching up, 0 before either. A page's own change leaves it
    /// alone, or that page's next level, sent with an older report, would be refused.
    floor: u64,
    /// Someone set this level: an earlier launch, a controller, the OS mixer, a plugin or a page.
    /// Until then the output plays the thread's full scale, nobody's choice, and TIDAL's saved
    /// level is restored and seeds it.
    chosen: bool,
}

/// Who changed the level: a page of ours, or something no page sees happen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LevelWriter {
    Page,
    Remote,
}

/// What Rust reports of its level: to TIDAL's store when a controller or the OS mixer moved it,
/// and to a page that loads, which cannot have seen those moves.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LevelReport {
    volume: f64,
    unmute_volume: f64,
    mute: bool,
    /// Someone chose this level: a loading page takes it, and TIDAL restores nothing over it.
    chosen: bool,
    seq: u64,
}

impl OutputLevel {
    pub(super) const START: Self = Self {
        volume: Volume(100.0),
        audible: Volume(100.0),
        seq: 0,
        floor: 0,
        chosen: false,
    };

    /// The level an earlier launch left, as [`LevelStore`] saved it. Stamped as a change no page of
    /// this launch has seen: a loading page takes it over TIDAL's, and a page level decided before
    /// taking it in is refused, as after a controller's move.
    pub(super) fn restored(volume: Volume, audible: Volume) -> Self {
        let mut level = Self::START;
        if !audible.is_silent() {
            level.audible = audible;
        }
        level.set(volume, LevelWriter::Remote);
        level
    }

    pub fn volume(self) -> Volume {
        self.volume
    }

    pub fn report(self) -> LevelReport {
        LevelReport {
            volume: self.volume.as_percent(),
            unmute_volume: self.audible.as_percent(),
            mute: self.volume.is_silent(),
            chosen: self.chosen,
            seq: self.seq,
        }
    }

    /// Whether a page's level may apply. `seen` is the newest report the page had taken in when
    /// it decided; one decided before the floor would undo a level that page never saw. A sender
    /// that states nothing (a plugin) is taken at its word.
    fn admits(self, seen: Option<u64>) -> bool {
        seen.is_none_or(|seen| seen >= self.floor)
    }

    /// A page that loads takes this level in. Every level decided before, by that page or any
    /// other, was decided without it; the page's own levels from here on carry this count.
    fn catch_up(&mut self) {
        self.seq += 1;
        self.floor = self.seq;
    }

    /// A page's level. One the output already plays changes nothing and is nobody's choice:
    /// TIDAL's native player applies the store's initial full scale when it is built, which must
    /// not stop TIDAL's saved level from seeding a level nobody chose yet. Returns whether the
    /// level changed.
    fn set_from_page(&mut self, volume: Volume) -> bool {
        if volume == self.volume {
            return false;
        }
        self.set(volume, LevelWriter::Page);
        true
    }

    fn stamp(&mut self, by: LevelWriter) {
        self.seq += 1;
        self.chosen = true;
        if by == LevelWriter::Remote {
            self.floor = self.seq;
        }
    }

    fn set(&mut self, volume: Volume, by: LevelWriter) {
        self.volume = volume;
        if !volume.is_silent() {
            self.audible = volume;
        }
        self.stamp(by);
    }

    /// Only a Connect controller mutes: TIDAL's own mute reaches Rust as a level of zero.
    fn mute(&mut self) {
        self.volume = Volume(0.0);
        self.stamp(LevelWriter::Remote);
    }

    /// A no-op on the level while audible: it is then the last audible one already.
    fn unmute(&mut self) {
        self.volume = self.audible;
        self.stamp(LevelWriter::Remote);
    }
}

/// Keeps [`OutputLevel`] across launches in `settings.db`, whoever set it. Written on every
/// change: a killed launch never reaches the quit path's flush. A write still queued takes the
/// newest level, not one write per step of a dragged slider or phone.
pub(super) struct LevelStore {
    pub(super) db: crate::db::DbActor,
    /// The newest level no queued write has taken yet: `Some` exactly while one write is queued.
    pub(super) unsaved: std::sync::Arc<std::sync::Mutex<Option<OutputLevel>>>,
}

impl LevelStore {
    /// Called under the `OutputLevel` lock, so levels arrive here in the order they were set.
    fn save(&self, level: OutputLevel) {
        if self
            .unsaved
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .replace(level)
            .is_some()
        {
            return;
        }
        let unsaved = std::sync::Arc::clone(&self.unsaved);
        self.db.post(move |_, settings| {
            let level = unsaved.lock().unwrap_or_else(|e| e.into_inner()).take();
            if let Some(level) = level {
                crate::settings::save_output_level(
                    settings,
                    level.volume.as_percent(),
                    level.audible.as_percent(),
                );
            }
        });
    }
}

impl Player {
    /// A page's level, dropped when the page decided it before taking in the level Rust plays
    /// ([`OutputLevel::admits`]). From here on it is a clamped, finite [`Volume`].
    pub fn set_volume(&self, volume: f64, seen: Option<u64>) -> anyhow::Result<()> {
        let volume = Volume::from_percent(volume);
        // Recorded and sent under one lock: a level recorded apart from its send could reach
        // the thread in the other order than the record says, and a mute would then save a
        // level the output never played.
        let mut level = self.output_level.lock().unwrap_or_else(|e| e.into_inner());
        if !level.admits(seen) {
            crate::vprintln!(
                "[VOLUME] page level {:.0}% dropped: decided before report {}",
                volume.as_percent(),
                level.floor
            );
            return Ok(());
        }
        if !level.set_from_page(volume) {
            return Ok(());
        }
        self.keep(*level);
        self.send_cmd(PlayerCommand::SetVolume(volume))
    }

    /// A page that loads takes in the level Rust plays, and gets the report its later levels
    /// carry. Levels decided before, by this page or another, are refused from here on.
    pub fn catch_up_page(&self) -> OutputLevel {
        let mut level = self.output_level.lock().unwrap_or_else(|e| e.into_inner());
        level.catch_up();
        *level
    }

    /// A Connect controller's level. Returns what the output now plays at, for the caller to
    /// report.
    pub fn set_remote_volume(&self, volume: f64) -> anyhow::Result<OutputLevel> {
        let volume = Volume::from_percent(volume);
        let mut level = self.output_level.lock().unwrap_or_else(|e| e.into_inner());
        level.set(volume, LevelWriter::Remote);
        self.keep(*level);
        self.send_cmd(PlayerCommand::SetVolume(volume))?;
        Ok(*level)
    }

    /// Every writer of the level passes here with the lock still held, which keeps the saved
    /// level the newest one: the next launch plays it, whoever set it.
    fn keep(&self, level: OutputLevel) {
        if let Some(store) = &self.level_store {
            store.save(level);
        }
    }

    /// Silences the output, or restores the last audible level it played. Returns what the
    /// output now plays at, which is what the caller reports: nothing else knows the level an
    /// unmute restores.
    pub fn set_mute(&self, mute: bool) -> anyhow::Result<OutputLevel> {
        let mut level = self.output_level.lock().unwrap_or_else(|e| e.into_inner());
        if mute {
            level.mute();
        } else {
            level.unmute();
        }
        self.keep(*level);
        self.send_cmd(PlayerCommand::SetVolume(level.volume()))?;
        Ok(*level)
    }

    /// The OS mixer moved this app's session. The thread already plays that level: it is only
    /// recorded, and the frontend adopts it without sending it back.
    pub fn note_volume(&self, volume: f64) -> OutputLevel {
        let mut level = self.output_level.lock().unwrap_or_else(|e| e.into_inner());
        level.set(Volume::from_percent(volume), LevelWriter::Remote);
        self.keep(*level);
        *level
    }

    pub fn output_level(&self) -> OutputLevel {
        *self.output_level.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
#[path = "../../tests/unit/player/volume.rs"]
mod tests;
