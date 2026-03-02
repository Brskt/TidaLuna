use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub(crate) const RESUME_FLUSH_INTERVAL: Duration = Duration::from_millis(1200);
pub(crate) const RESUME_MIN_SECONDS: f64 = 1.0;

fn resume_store_path() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        let base = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(base)
            .join("tidal-rs")
            .join("resume_positions.json")
    }
    #[cfg(not(target_os = "windows"))]
    {
        let base = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(base)
            .join(".local")
            .join("share")
            .join("tidal-rs")
            .join("resume_positions.json")
    }
}

pub(crate) struct ResumeStore {
    path: PathBuf,
    positions: HashMap<String, f64>,
    dirty: bool,
    last_flush: Instant,
}

impl ResumeStore {
    pub fn load() -> Self {
        let path = resume_store_path();
        let positions = fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<HashMap<String, f64>>(&bytes).ok())
            .map(|map| {
                map.into_iter()
                    .filter(|(_, v)| v.is_finite() && *v > RESUME_MIN_SECONDS)
                    .collect::<HashMap<_, _>>()
            })
            .unwrap_or_default();

        Self {
            path,
            positions,
            dirty: false,
            last_flush: Instant::now(),
        }
    }

    pub fn get(&self, track_id: &str) -> Option<f64> {
        self.positions
            .get(track_id)
            .copied()
            .filter(|v| v.is_finite() && *v > RESUME_MIN_SECONDS)
    }

    pub fn set(&mut self, track_id: &str, seconds: f64) {
        if !seconds.is_finite() || seconds <= RESUME_MIN_SECONDS {
            return;
        }
        let old = self.positions.get(track_id).copied().unwrap_or(0.0);
        if (old - seconds).abs() >= 0.25 {
            self.positions.insert(track_id.to_string(), seconds);
            self.dirty = true;
        }
    }

    pub fn clear(&mut self, track_id: &str) {
        if self.positions.remove(track_id).is_some() {
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
            eprintln!("[RESUME] Failed to create state directory: {e}");
            return;
        }

        match serde_json::to_vec(&self.positions) {
            Ok(buf) => {
                if let Err(e) = fs::write(&self.path, buf) {
                    eprintln!("[RESUME] Failed to persist state: {e}");
                } else {
                    self.dirty = false;
                    self.last_flush = Instant::now();
                }
            }
            Err(e) => eprintln!("[RESUME] Failed to serialize state: {e}"),
        }
    }
}
