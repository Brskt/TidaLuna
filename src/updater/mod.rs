mod check;
mod download;
mod handlers;
mod highwater;
mod recovery;
mod types;
mod util;
mod verify;

pub(crate) use check::trigger_update_check;
pub(crate) use handlers::{
    handle_updater_apply, handle_updater_cancel, handle_updater_check, handle_updater_dismiss,
    handle_updater_download, handle_updater_status,
};
pub(crate) use recovery::recover_interrupted_update;

/// Record the running build's version as the anti-rollback high-water mark.
/// Call once at startup after a successful boot.
pub(crate) fn record_launch_version() {
    highwater::record(&crate::state::cache_data_dir(), env!("CARGO_PKG_VERSION"));
}

const GITHUB_OWNER: &str = "Brskt";
const GITHUB_REPO: &str = "TidaLuna";

const TARGET: &str = {
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    {
        "windows-amd64"
    }
    #[cfg(all(target_os = "windows", target_arch = "aarch64"))]
    {
        "windows-arm64"
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        "linux-amd64"
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    {
        "linux-arm64"
    }
    #[cfg(not(any(
        all(target_os = "windows", target_arch = "x86_64"),
        all(target_os = "windows", target_arch = "aarch64"),
        all(target_os = "linux", target_arch = "x86_64"),
        all(target_os = "linux", target_arch = "aarch64"),
    )))]
    {
        "unsupported"
    }
};

const UPDATER_EXE: &str = if cfg!(target_os = "windows") {
    "updater.exe"
} else {
    "updater"
};

fn manifest_name() -> String {
    format!("manifest-{TARGET}.json")
}

/// Platform suffix for the release archive: `{os}_{arch}.{ext}`. Windows ships
/// a flat `.zip`, Linux a `.tar.gz` whose entries are wrapped in a single
/// top-level directory (stripped at extraction time). These are the same
/// user-facing archives published on the Releases page.
const ARCHIVE_SUFFIX: &str = {
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    {
        "win32_x64.zip"
    }
    #[cfg(all(target_os = "windows", target_arch = "aarch64"))]
    {
        "win32_arm64.zip"
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        "linux_amd64.tar.gz"
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    {
        "linux_arm64.tar.gz"
    }
    #[cfg(not(any(
        all(target_os = "windows", target_arch = "x86_64"),
        all(target_os = "windows", target_arch = "aarch64"),
        all(target_os = "linux", target_arch = "x86_64"),
        all(target_os = "linux", target_arch = "aarch64"),
    )))]
    {
        "unsupported"
    }
};

/// Release archive asset name for a given version, e.g.
/// `tidalunar_0.0.4-alpha_win32_x64.zip`.
fn archive_name(version: &str) -> String {
    format!("tidalunar_{version}_{ARCHIVE_SUFFIX}")
}

/// Delta archive asset name for a version, e.g.
/// `tidalunar_0.0.6-alpha_update_win32_x64.zip`.
fn delta_archive_name(version: &str) -> String {
    format!("tidalunar_{version}_update_{ARCHIVE_SUFFIX}")
}
