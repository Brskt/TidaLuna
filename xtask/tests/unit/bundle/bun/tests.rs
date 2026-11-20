//! Tests for `xtask/src/bundle/bun.rs`, attached to it by `#[path]`.

use std::io::Write as _;

use super::extract_entry;

/// A fresh directory under the system temp dir; xtask carries no `tempfile`.
fn scratch(tag: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("xtask-{tag}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A zip holding `name`, deflated as Oven ships Bun.
fn zip_with(dir: &std::path::Path, name: &str, data: &[u8]) -> std::path::PathBuf {
    let path = dir.join("bun.zip");
    let mut zip = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
    let opts: zip::write::FileOptions<()> =
        zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    zip.start_file(name, opts).unwrap();
    zip.write_all(data).unwrap();
    zip.finish().unwrap();
    path
}

#[test]
fn the_named_entry_is_extracted_whole() {
    let dir = scratch("bun-extract");
    let zip = zip_with(&dir, "bun-linux-x64/bun", b"bun binary");
    let dst = dir.join("bun-v1");
    extract_entry(&zip, "bun-linux-x64/bun", &dst).unwrap();
    assert_eq!(std::fs::read(&dst).unwrap(), b"bun binary");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// The bundle fails rather than ship without Bun.
#[test]
fn an_archive_without_the_entry_is_an_error_and_leaves_no_binary() {
    let dir = scratch("bun-missing");
    let zip = zip_with(&dir, "bun-linux-x64/README.md", b"no binary here");
    let dst = dir.join("bun-v1");
    assert!(extract_entry(&zip, "bun-linux-x64/bun", &dst).is_err());
    assert!(!dst.exists(), "a binary was left behind");
    std::fs::remove_dir_all(&dir).unwrap();
}
