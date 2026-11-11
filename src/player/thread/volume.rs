use super::PlayerThread;
use crate::player::PlayerEvent;
use std::sync::atomic::Ordering::Relaxed;
#[cfg(target_os = "windows")]
use std::sync::mpsc;

impl<F: Fn(PlayerEvent) + Send + 'static> PlayerThread<F> {
    pub(super) fn handle_set_volume(&mut self, vol: crate::player::Volume) {
        let vol_f32 = vol.as_gain();
        // Record the real UI level on every path (incl. the session-sync Ok path
        // below, which pins self.volume to 1.0). The reliable seed for the
        // exclusive digital gain when volume_sync.get() later fails on switch.
        #[cfg(target_os = "windows")]
        self.last_volume.store(f32::to_bits(vol_f32), Relaxed);
        // Exclusive and ASIO bypass the OS session mixer; drive the shared digital
        // gain (the ASIO control thread reads the same cell as the WASAPI render).
        #[cfg(target_os = "windows")]
        if self.is_exclusive_mode || self.is_asio_mode {
            self.exclusive_gain.store(f32::to_bits(vol_f32), Relaxed);
        }
        #[cfg(target_os = "windows")]
        if let Some(ref vs) = self.volume_sync {
            match vs.set(vol_f32) {
                Ok(()) => {
                    self.volume.store(f32::to_bits(1.0), Relaxed);
                    return;
                }
                Err(_) => {
                    crate::vprintln!("[VOLUME] Session set failed, falling back to software gain");
                }
            }
            self.volume_sync = None;
            self.volume_rx = None;
        }
        self.volume.store(f32::to_bits(vol_f32), Relaxed);
    }

    #[cfg(target_os = "windows")]
    pub(super) fn init_volume_sync(&mut self) {
        if self._com_guard.is_none() || !self.volume_sync_enabled {
            return;
        }

        let device_id = self.current_device_id.as_deref().unwrap_or("default");

        let (tx, rx) = mpsc::channel();
        match crate::platform::volume_sync::VolumeSync::new(device_id, tx) {
            Ok(vs) => {
                if self.volume_baseline_established {
                    // Re-init (mode/device switch), or a first init after a restored
                    // level (`PlayerThread::new`): assert our known level into the
                    // (possibly fresh) session instead of adopting its GetMasterVolume.
                    // A never-set session reports 1.0 by default (MS docs), which would
                    // poison last_volume to full scale and blast the exclusive/ASIO gain.
                    let app_vol = f32::from_bits(self.last_volume.load(Relaxed));
                    if let Err(e) = vs.set(app_vol) {
                        // Failed re-assert: do NOT trust the session (a never-set one
                        // sits at 1.0) and do NOT commit sync. Fall back to software
                        // gain at the app's level: same policy as the re-enable path
                        // in handle_set_volume_sync.
                        self.volume.store(f32::to_bits(app_vol), Relaxed);
                        crate::vprintln!(
                            "[VOLUME] Session re-assert failed: {e}; using software gain"
                        );
                        return;
                    }
                    self.volume.store(f32::to_bits(1.0), Relaxed);
                    crate::vprintln!(
                        "[VOLUME] Session sync re-asserted at {:.0}%",
                        app_vol * 100.0
                    );
                } else {
                    // Cold start: adopt the persisted session volume as the baseline and
                    // record it in last_volume directly (don't wait on the UI echo).
                    match vs.get() {
                        Ok(initial) => {
                            self.last_volume.store(f32::to_bits(initial), Relaxed);
                            self.volume_baseline_established = true;
                            let level = (initial * 100.0) as f64;
                            (self.callback)(PlayerEvent::VolumeSync(level));
                            self.volume.store(f32::to_bits(1.0), Relaxed);
                            crate::vprintln!(
                                "[VOLUME] Session sync active, initial level: {:.0}%",
                                level
                            );
                        }
                        Err(e) => {
                            crate::vprintln!(
                                "[VOLUME] Initial get failed: {e}, disabling OS volume sync"
                            );
                            return;
                        }
                    }
                }
                self.volume_sync = Some(vs);
                self.volume_rx = Some(rx);
            }
            Err(e) => {
                crate::vprintln!("[VOLUME] VolumeSync init failed: {e}, using software gain");
            }
        }
    }

    #[cfg(target_os = "windows")]
    pub(super) fn handle_set_volume_sync(&mut self, enabled: bool) {
        self.volume_sync_enabled = enabled;
        if enabled {
            if self.cpal_stream.is_some() && self._com_guard.is_some() {
                let app_vol = f32::from_bits(self.volume.load(Relaxed));
                let device_id = self.current_device_id.as_deref().unwrap_or("default");
                let (tx, rx) = mpsc::channel();
                match crate::platform::volume_sync::VolumeSync::new(device_id, tx) {
                    Ok(vs) => {
                        if let Err(e) = vs.set(app_vol) {
                            crate::vprintln!(
                                "[VOLUME] set failed on re-enable: {e}, staying on software gain"
                            );
                            return;
                        }
                        self.volume.store(f32::to_bits(1.0), Relaxed);
                        self.volume_sync = Some(vs);
                        self.volume_rx = Some(rx);
                        crate::vprintln!(
                            "[VOLUME] Session sync re-enabled at {:.0}%",
                            app_vol * 100.0
                        );
                    }
                    Err(e) => {
                        crate::vprintln!("[VOLUME] VolumeSync init failed on re-enable: {e}");
                    }
                }
            }
        } else if let Some(ref vs) = self.volume_sync {
            let level = match vs.get() {
                Ok(l) => l,
                Err(e) => {
                    crate::vprintln!("[VOLUME] Cannot disable sync: get() failed: {e}");
                    self.volume_sync_enabled = true;
                    return;
                }
            };
            // Mute cpal: the audio buffer has samples produced with software_gain=1.0.
            // Setting session to 1.0 would spike those to max. Muting lets at least
            // one callback drain the stale buffer before unmute (via mute_ack).
            if let Some(ref muted) = self.cpal_muted {
                muted.store(true, Relaxed);
            }
            self.volume.store(f32::to_bits(level), Relaxed);
            if let Err(e) = vs.set(1.0) {
                self.volume.store(f32::to_bits(1.0), Relaxed);
                if let Some(ref muted) = self.cpal_muted {
                    muted.store(false, Relaxed);
                }
                crate::vprintln!("[VOLUME] Cannot disable sync: set(1.0) failed: {e}");
                self.volume_sync_enabled = true;
                return;
            }
            if let Some(ref ack) = self.cpal_mute_ack {
                ack.store(false, Relaxed);
            }
            self.pending_unmute = true;
            self.volume_sync = None;
            self.volume_rx = None;
            crate::vprintln!(
                "[VOLUME] Session sync disabled, transferred {:.0}% to software gain",
                level * 100.0
            );
        }
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/player/thread/volume.rs"]
mod tests;
