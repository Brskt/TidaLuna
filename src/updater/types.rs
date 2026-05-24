use std::collections::BTreeMap;
use std::sync::LazyLock;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex as TokioMutex;
use tokio_util::sync::CancellationToken;

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
    #[allow(dead_code)]
    Applying(String),
}

pub(crate) struct UpdaterState {
    pub phase: UpdaterPhase,
    pub task: Option<tokio::task::JoinHandle<()>>,
    pub cancel: Option<CancellationToken>,
    pub last_info: Option<UpdateInfo>,
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
}

pub(crate) static UPDATER_STATE: LazyLock<TokioMutex<UpdaterState>> = LazyLock::new(|| {
    TokioMutex::new(UpdaterState {
        phase: UpdaterPhase::Idle,
        task: None,
        cancel: None,
        last_info: None,
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
mod tests {
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
}
