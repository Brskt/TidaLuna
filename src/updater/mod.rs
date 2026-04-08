mod check;
mod download;
mod handlers;
mod recovery;
mod types;
mod util;

pub(crate) use check::trigger_update_check;
pub(crate) use handlers::{
    handle_updater_apply, handle_updater_cancel, handle_updater_check, handle_updater_dismiss,
    handle_updater_download, handle_updater_status,
};
pub(crate) use recovery::recover_interrupted_update;

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

fn zip_name() -> String {
    format!("tidalunar-{TARGET}.zip")
}
