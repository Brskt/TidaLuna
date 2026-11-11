#[cfg(any(target_os = "windows", target_os = "linux"))]
use std::fs;
use std::path::Path;

#[cfg(any(target_os = "windows", target_os = "linux"))]
use anyhow::Context;
use anyhow::Result;

#[cfg(any(target_os = "windows", target_os = "linux"))]
use crate::manifest::is_safe_relative_path;

pub(crate) fn extract_archive(archive_path: &Path, dest_dir: &Path) -> Result<()> {
    #[cfg(target_os = "windows")]
    return extract_zip(archive_path, dest_dir);
    #[cfg(target_os = "linux")]
    return extract_tar_gz(archive_path, dest_dir);
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        let _ = (archive_path, dest_dir);
        anyhow::bail!("update extraction unsupported on this platform");
    }
}

#[cfg(target_os = "windows")]
fn extract_zip(zip_path: &Path, dest_dir: &Path) -> Result<()> {
    let file = fs::File::open(zip_path).context("open zip")?;
    let mut archive = zip::ZipArchive::new(file).context("parse zip")?;

    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).context("zip entry")?;
        let name = entry.name().context("zip entry name")?.into_owned();

        if !is_safe_relative_path(&name, dest_dir) {
            anyhow::bail!("zip entry has unsafe path: {name}");
        }

        // Skip directories
        if entry.is_dir() {
            let dir_path = dest_dir.join(&name);
            fs::create_dir_all(&dir_path).ok();
            continue;
        }

        let out_path = dest_dir.join(&name);
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent).ok();
        }

        let mut out_file = fs::File::create(&out_path)
            .with_context(|| format!("create {}", out_path.display()))?;
        std::io::copy(&mut entry, &mut out_file).with_context(|| format!("extract {name}"))?;
    }
    Ok(())
}

/// Extract a `.tar.gz` whose entries are wrapped in a single top-level
/// directory, stripping it and leaving files at `dest_dir` root to match the
/// manifest's relative paths. Unix modes from the tar header are preserved.
#[cfg(target_os = "linux")]
fn extract_tar_gz(archive_path: &Path, dest_dir: &Path) -> Result<()> {
    let file = fs::File::open(archive_path).context("open tar.gz")?;
    let decoder = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(decoder);

    for entry in archive.entries().context("read tar entries")? {
        let mut entry = entry.context("tar entry")?;
        let path = entry.path().context("tar entry path")?.into_owned();

        // Drop the single leading component (tidalunar_{version}_linux_{arch}/).
        let mut comps = path.components();
        comps.next();
        let rel = comps.as_path();
        if rel.as_os_str().is_empty() {
            continue;
        }

        let rel_str = rel.to_string_lossy();
        if !is_safe_relative_path(&rel_str, dest_dir) {
            anyhow::bail!("tar entry has unsafe path: {}", path.display());
        }

        let out_path = dest_dir.join(rel);
        let etype = entry.header().entry_type();
        if etype.is_dir() {
            fs::create_dir_all(&out_path).ok();
            continue;
        }
        if !etype.is_file() {
            // Plain files only; reject symlinks/hardlinks/devices (traversal
            // vector in an archive not hash-bound before extraction).
            anyhow::bail!("tar entry {rel_str} has disallowed type {etype:?}");
        }
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent).ok();
        }
        entry
            .unpack(&out_path)
            .with_context(|| format!("extract {rel_str}"))?;
    }
    Ok(())
}
