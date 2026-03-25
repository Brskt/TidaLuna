use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

const RESUME_FLUSH_INTERVAL: Duration = Duration::from_millis(1200);
pub(crate) const RESUME_MIN_SECONDS: f64 = 1.0;

fn resume_store_path() -> PathBuf {
    crate::state::cache_data_dir().join("resume_position.json")
}

#[derive(Serialize, Deserialize)]
struct ResumeEntry {
    track_id: String,
    position: f64,
}

pub(crate) struct ResumeStore {
    path: PathBuf,
    entry: Option<ResumeEntry>,
    dirty: bool,
    last_flush: Instant,
}

impl ResumeStore {
    pub fn load() -> Self {
        let path = resume_store_path();
        let entry = fs::read(&path)
            .ok()
            .and_then(|bytes| {
                // Try new single-entry format first
                serde_json::from_slice::<ResumeEntry>(&bytes)
                    .ok()
                    .or_else(|| {
                        // Migrate from old HashMap format (resume_positions.json)
                        let old: HashMap<String, f64> = serde_json::from_slice(&bytes).ok()?;
                        let (track_id, position) = old
                            .into_iter()
                            .filter(|(_, v)| v.is_finite() && *v > RESUME_MIN_SECONDS)
                            .last()?;
                        crate::vprintln!("[RESUME] Migrated from old format");
                        Some(ResumeEntry { track_id, position })
                    })
            })
            .filter(|e| e.position.is_finite() && e.position > RESUME_MIN_SECONDS);

        Self {
            path,
            entry,
            dirty: false,
            last_flush: Instant::now(),
        }
    }

    pub fn get(&self, track_id: &str) -> Option<f64> {
        self.entry
            .as_ref()
            .filter(|e| {
                e.track_id == track_id && e.position.is_finite() && e.position > RESUME_MIN_SECONDS
            })
            .map(|e| e.position)
    }

    pub fn set(&mut self, track_id: &str, seconds: f64) {
        if !seconds.is_finite() || seconds <= RESUME_MIN_SECONDS {
            return;
        }
        let old = self
            .entry
            .as_ref()
            .filter(|e| e.track_id == track_id)
            .map(|e| e.position)
            .unwrap_or(0.0);
        if (old - seconds).abs() >= 0.25
            || self.entry.as_ref().is_none_or(|e| e.track_id != track_id)
        {
            self.entry = Some(ResumeEntry {
                track_id: track_id.to_string(),
                position: seconds,
            });
            self.dirty = true;
        }
    }

    pub fn clear(&mut self, track_id: &str) {
        if self.entry.as_ref().is_some_and(|e| e.track_id == track_id) {
            self.entry = None;
            self.dirty = true;
        }
    }

    pub fn flush_if_due(&mut self, force: bool) {
        if !self.dirty {
            return;
        }
        if !force && self.last_flush.elapsed() < RESUME_FLUSH_INTERVAL {
            return;
        }

        if let Some(parent) = self.path.parent()
            && let Err(e) = fs::create_dir_all(parent)
        {
            crate::vprintln!("[RESUME] Failed to create state directory: {e}");
            return;
        }

        match serde_json::to_vec(&self.entry) {
            Ok(buf) => {
                if let Err(e) = fs::write(&self.path, buf) {
                    crate::vprintln!("[RESUME] Failed to persist state: {e}");
                } else {
                    self.dirty = false;
                    self.last_flush = Instant::now();
                }
            }
            Err(e) => crate::vprintln!("[RESUME] Failed to serialize state: {e}"),
        }
    }
}
