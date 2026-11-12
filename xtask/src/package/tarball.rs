use std::fs;
use std::path::Path;
use std::process::Command;

use super::deb::deb_file_name;
use super::render_template;
use crate::manifest::Manifest;
use crate::{project_root, read_workspace_version, run_status};

/// The tarball's top folder, and its file name before `.tar.gz`.
fn tarball_bundle_name(version: &str, arch: &str) -> String {
    format!("tidalunar_{version}_linux_{arch}")
}

pub(super) fn package_linux_tarball(arch: &str) -> Result<(), String> {
    let project_root = project_root()?;
    let dist = project_root.join("dist");
    if !dist.is_dir() {
        return Err("dist/ missing - run `cargo xtask bundle` first".into());
    }

    let manifest_data =
        fs::read_to_string(dist.join("manifest.json")).map_err(|e| e.to_string())?;
    let manifest: Manifest = serde_json::from_str(&manifest_data).map_err(|e| e.to_string())?;
    let expected = format!("linux-{arch}");
    if manifest.target != expected {
        return Err(format!(
            "dist/ payload target {} does not match --arch {} (expected {})",
            manifest.target, arch, expected
        ));
    }

    let version = read_workspace_version()?;
    let out_dir = project_root.join("target").join("installer");
    fs::create_dir_all(&out_dir).map_err(|e| e.to_string())?;

    let stage_root = out_dir.join(format!("tarball-build-{arch}"));
    if stage_root.exists() {
        fs::remove_dir_all(&stage_root).map_err(|e| e.to_string())?;
    }
    let bundle_name = tarball_bundle_name(&version, arch);
    let stage = stage_root.join(&bundle_name);
    fs::create_dir_all(&stage).map_err(|e| e.to_string())?;

    // Recursively copy dist/ into stage/.
    copy_dir_all(&dist, &stage)?;

    // Drop in the README, naming this archive and the .deb as they are built.
    let readme_template = fs::read_to_string(
        project_root
            .join("installer")
            .join("linux")
            .join("tarball")
            .join("README"),
    )
    .map_err(|e| format!("read README: {e}"))?;
    let readme = render_template(
        &readme_template,
        &[
            ("BUNDLE", &bundle_name),
            ("DEB", &deb_file_name(&version, arch)),
        ],
    )
    .map_err(|e| format!("README: {e}"))?;
    fs::write(stage.join("README"), readme).map_err(|e| format!("write README: {e}"))?;

    let out_tarball = out_dir.join(format!("{bundle_name}.tar.gz"));
    run_status(
        Command::new("tar")
            .arg("-czf")
            .arg(&out_tarball)
            .arg("-C")
            .arg(&stage_root)
            .arg(&bundle_name),
        "tar",
    )?;

    println!(".tar.gz created: {}", out_tarball.display());
    Ok(())
}

fn copy_dir_all(src: &Path, dst: &Path) -> Result<(), String> {
    fs::create_dir_all(dst).map_err(|e| e.to_string())?;
    for entry in fs::read_dir(src).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if from.is_dir() {
            copy_dir_all(&from, &to)?;
        } else {
            fs::copy(&from, &to).map_err(|e| format!("copy {}: {e}", from.display()))?;
        }
    }
    Ok(())
}
