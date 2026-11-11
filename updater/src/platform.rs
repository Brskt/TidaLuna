#[cfg(target_os = "linux")]
use std::fs;

#[cfg(target_os = "linux")]
use crate::Manifest;

pub(crate) const EXE_NAME: &str = if cfg!(target_os = "windows") {
    "tidalunar.exe"
} else {
    "tidalunar"
};

pub(crate) const TARGET: &str = {
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

/// Delta archive asset name for a version, e.g.
/// `tidalunar_0.0.6-alpha_update_win32_x64.zip`.
pub(crate) fn delta_archive_name(version: &str) -> String {
    format!("tidalunar_{version}_update_{ARCHIVE_SUFFIX}")
}

/// Platform suffix for the release archive (`{os}_{arch}.{ext}`). Mirrors
/// `src/updater/mod.rs::ARCHIVE_SUFFIX`. Windows ships a flat `.zip`, Linux a
/// `.tar.gz` whose entries are wrapped in one top-level directory.
pub(crate) const ARCHIVE_SUFFIX: &str = {
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

/// Read /usr/lib/tidalunar/SANDBOX_PROTOCOL_VERSION. `None` = file absent
/// (not a packaged .deb install: tar.gz/dev, gate N/A); `Some(0)` = present but
/// malformed. Mirrors src/updater/util.rs::read_system_sandbox_protocol; the
/// standalone updater is intentionally dependency-free and duplicates it.
#[cfg(target_os = "linux")]
fn read_system_sandbox_protocol() -> Option<u32> {
    fs::read_to_string("/usr/lib/tidalunar/SANDBOX_PROTOCOL_VERSION")
        .ok()
        .map(|s| s.trim().parse::<u32>().unwrap_or(0))
}

#[cfg(target_os = "linux")]
pub(crate) fn enforce_sandbox_protocol_gate(manifest: &Manifest) -> Result<(), anyhow::Error> {
    use anyhow::bail;
    // No system protocol file -> not a .deb install -> gate does not apply.
    let Some(system) = read_system_sandbox_protocol() else {
        return Ok(());
    };
    let required = manifest.sandbox_protocol_required.unwrap_or(0);
    if required > system {
        bail!(
            "Update v{} requires sandbox helper protocol {}, but system has {}. \
             Run 'sudo apt upgrade tidalunar' and re-launch.",
            manifest.version,
            required,
            system,
        );
    }
    Ok(())
}
