use std::fs;
use std::path::PathBuf;
use std::process::Command;

use crate::manifest::Manifest;
use crate::project_root;

/// Files in `new` that are new or whose sha256 differs from `old`. Files only
/// in `old` (removed) are not returned; deletions are handled by the updater's
/// manifest diff at apply time, not by the delta archive.
fn delta_changed_files(old: &Manifest, new: &Manifest) -> Vec<String> {
    new.files
        .iter()
        .filter(|(path, entry)| {
            old.files
                .get(path.as_str())
                .map(|o| o.sha256 != entry.sha256)
                .unwrap_or(true)
        })
        .map(|(path, _)| path.clone())
        .collect()
}

pub(crate) fn delta(flags: &[String]) -> Result<(), String> {
    let mut target: Option<String> = None;
    let mut arch: Option<String> = None;
    let mut old_manifest: Option<String> = None;
    let mut dist: Option<String> = None;
    let mut i = 0;
    while i < flags.len() {
        match flags[i].as_str() {
            "--target" => target = Some(crate::flag_value(flags, &mut i)?),
            "--arch" => arch = Some(crate::flag_value(flags, &mut i)?),
            "--old-manifest" => old_manifest = Some(crate::flag_value(flags, &mut i)?),
            "--dist" => dist = Some(crate::flag_value(flags, &mut i)?),
            other => return Err(format!("unknown delta flag: {other}")),
        }
        i += 1;
    }
    let target = target.ok_or("--target is required (win32|linux)")?;
    let arch = arch.ok_or("--arch is required (amd64|arm64)")?;
    if arch != "amd64" && arch != "arm64" {
        return Err(format!("--arch must be amd64 or arm64, got: {arch}"));
    }
    let old_manifest_path = old_manifest.ok_or("--old-manifest is required")?;

    let project_root = project_root()?;
    let dist_dir = match dist {
        Some(d) => PathBuf::from(d),
        None => project_root.join("dist"),
    };

    let new: Manifest = serde_json::from_str(
        &fs::read_to_string(dist_dir.join("manifest.json"))
            .map_err(|e| format!("read new manifest: {e}"))?,
    )
    .map_err(|e| format!("parse new manifest: {e}"))?;
    let old: Manifest = serde_json::from_str(
        &fs::read_to_string(&old_manifest_path).map_err(|e| format!("read old manifest: {e}"))?,
    )
    .map_err(|e| format!("parse old manifest: {e}"))?;

    let changed = delta_changed_files(&old, &new);
    println!(
        "delta: {} changed/new files vs {}",
        changed.len(),
        old.version
    );

    // Token + extension mirror src/updater/mod.rs::ARCHIVE_SUFFIX.
    let (token, ext) = match (target.as_str(), arch.as_str()) {
        ("win32", "amd64") => ("win32_x64", "zip"),
        ("win32", "arm64") => ("win32_arm64", "zip"),
        ("linux", "amd64") => ("linux_amd64", "tar.gz"),
        ("linux", "arm64") => ("linux_arm64", "tar.gz"),
        _ => {
            return Err(format!(
                "unsupported --target/--arch combo: {target}/{arch}"
            ));
        }
    };
    let version = new.version.clone();
    let out_dir = project_root.join("target").join("installer");
    fs::create_dir_all(&out_dir).map_err(|e| e.to_string())?;
    let out = out_dir.join(format!("tidalunar_{version}_update_{token}.{ext}"));

    match target.as_str() {
        "linux" => {
            // Wrapped top-level dir, matching the full tarball. The updater's
            // strip-1 extraction works unchanged.
            let wrap = format!("tidalunar_{version}_update_{token}");
            let stage_root = out_dir.join(format!("delta-build-{token}"));
            if stage_root.exists() {
                fs::remove_dir_all(&stage_root).map_err(|e| e.to_string())?;
            }
            let stage = stage_root.join(&wrap);
            for rel in &changed {
                let from = dist_dir.join(rel);
                let to = stage.join(rel);
                if let Some(p) = to.parent() {
                    fs::create_dir_all(p).map_err(|e| e.to_string())?;
                }
                fs::copy(&from, &to).map_err(|e| format!("copy {rel}: {e}"))?;
            }
            let status = Command::new("tar")
                .args(["-czf"])
                .arg(&out)
                .args(["-C"])
                .arg(&stage_root)
                .arg(&wrap)
                .status()
                .map_err(|e| format!("tar spawn: {e}"))?;
            if !status.success() {
                return Err(format!("tar failed: {status}"));
            }
        }
        "win32" => {
            // Flat zip, matching the full windows bundle.
            use std::io::Write as _;
            let file = fs::File::create(&out).map_err(|e| format!("create zip: {e}"))?;
            let mut zip = zip::ZipWriter::new(file);
            let opts: zip::write::FileOptions<()> = zip::write::FileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            for rel in &changed {
                let data = fs::read(dist_dir.join(rel)).map_err(|e| format!("read {rel}: {e}"))?;
                zip.start_file(rel.replace('\\', "/"), opts)
                    .map_err(|e| e.to_string())?;
                zip.write_all(&data).map_err(|e| e.to_string())?;
            }
            zip.finish().map_err(|e| e.to_string())?;
        }
        _ => unreachable!(),
    }
    println!("delta archive created: {}", out.display());
    Ok(())
}

#[cfg(test)]
#[path = "../tests/unit/delta/delta_diff_tests.rs"]
mod delta_diff_tests;
