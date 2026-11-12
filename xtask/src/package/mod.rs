use std::fs;
use std::path::Path;

use crate::manifest::Manifest;

mod deb;
mod nsis;
mod tarball;

/// Fills each `{KEY}` of an installer template. A key the template does not hold is an error: the
/// value would be shipped nowhere, and a template edited by hand drifts from what the code knows.
pub(super) fn render_template(template: &str, pairs: &[(&str, &str)]) -> Result<String, String> {
    let mut out = template.to_string();
    for (key, value) in pairs {
        let token = format!("{{{key}}}");
        if !out.contains(&token) {
            return Err(format!("template has no {token}"));
        }
        out = out.replace(&token, value);
    }
    Ok(out)
}

/// Refuses a `dist/` that `bundle` and `build-updater` have not filled, then checks its payload
/// target with [`payload_target`].
fn check_dist(dist: &Path, updater: &str, os: &str, arch: &str) -> Result<String, String> {
    if !dist.is_dir()
        || fs::read_dir(dist)
            .map_err(|e| e.to_string())?
            .next()
            .is_none()
    {
        return Err("dist/ is empty or missing - run `cargo xtask bundle` first".into());
    }
    if !dist.join("manifest.json").is_file() {
        return Err("dist/manifest.json missing - run `cargo xtask bundle` first".into());
    }
    if !dist.join(updater).is_file() {
        return Err(format!(
            "dist/{updater} missing - run `cargo xtask build-updater` first"
        ));
    }
    payload_target(dist, os, arch)
}

/// The `{os}-{arch}` target, once `dist/manifest.json` is checked to carry it.
fn payload_target(dist: &Path, os: &str, arch: &str) -> Result<String, String> {
    let manifest_data =
        fs::read_to_string(dist.join("manifest.json")).map_err(|e| e.to_string())?;
    let manifest: Manifest = serde_json::from_str(&manifest_data).map_err(|e| e.to_string())?;
    let expected = format!("{os}-{arch}");
    if manifest.target != expected {
        return Err(format!(
            "dist/ payload target {} does not match --arch {} (expected {})",
            manifest.target, arch, expected
        ));
    }
    Ok(expected)
}

pub(crate) fn package(flags: &[String]) -> Result<(), String> {
    let mut arch: Option<String> = None;
    let mut target: String = "windows-nsis".into();
    let mut i = 0;
    while i < flags.len() {
        match flags[i].as_str() {
            "--release" => {} // payload was built by `bundle`
            "--target" => target = crate::flag_value(flags, &mut i)?,
            "--arch" => arch = Some(crate::flag_value(flags, &mut i)?),
            other => return Err(format!("unknown argument: {other}")),
        }
        i += 1;
    }
    let arch = arch.ok_or(
        "--arch <amd64|arm64> is required (no host-default to avoid cross-build mismatch)",
    )?;
    if arch != "amd64" && arch != "arm64" {
        return Err(format!("--arch must be amd64 or arm64, got: {arch}"));
    }

    match target.as_str() {
        "windows-nsis" => nsis::package_windows_nsis(&arch),
        "linux-deb" => deb::package_linux_deb(&arch),
        "linux-tarball" => tarball::package_linux_tarball(&arch),
        other => Err(format!(
            "unknown --target: {other} (expected windows-nsis | linux-deb | linux-tarball)"
        )),
    }
}
