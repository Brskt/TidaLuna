//! Tests for `src/frontend/load.rs`, attached by `#[path]`.

use std::path::Path;

use super::super::dir::{DirOrigin, FrontendDir};
use super::super::fixture::write_front;
use super::{EARLY_RUNTIME_FILES, FrontendError, load};

fn complete() -> tempfile::TempDir {
    let root = tempfile::tempdir().expect("tempdir");
    write_front(root.path(), env!("CARGO_PKG_VERSION"));
    root
}

fn shipped(dir: &Path) -> FrontendDir {
    FrontendDir {
        path: dir.to_path_buf(),
        origin: DirOrigin::Shipped,
    }
}

#[test]
fn only_an_end_marker_line_in_the_host_is_refused() {
    for host in ["a;\n__END__\nb;\n", "a;\r\n__END__\r\nb;\r\n"] {
        let root = complete();
        std::fs::write(root.path().join("native-host.cjs"), host).expect("host");
        let err = load(&shipped(root.path())).err().expect("refused");
        assert!(
            matches!(err, FrontendError::EndMarker(_)),
            "{host:?}: {err:?}"
        );
    }
    let root = complete();
    std::fs::write(root.path().join("native-host.cjs"), "var x = 'x__END__';\n").expect("host");
    assert!(load(&shipped(root.path())).is_ok());
}

#[test]
fn a_shipped_version_mismatch_refuses() {
    let root = complete();
    std::fs::write(
        root.path().join("frontend.json"),
        "{\"version\":\"0.0.0-other\"}",
    )
    .expect("manifest");
    let err = load(&shipped(root.path())).err().expect("refused");
    assert!(
        matches!(&err, FrontendError::VersionMismatch { found, expected }
            if found == "0.0.0-other" && *expected == env!("CARGO_PKG_VERSION")),
        "{err:?}"
    );
}

#[test]
fn the_early_runtime_list_matches_the_source_folder() {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("frontend/early-runtime");
    let mut on_disk: Vec<String> = std::fs::read_dir(&source)
        .expect("frontend/early-runtime exists")
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|name| name.ends_with(".js"))
        .collect();
    on_disk.sort();
    let mut listed: Vec<String> = EARLY_RUNTIME_FILES.iter().map(|s| s.to_string()).collect();
    listed.sort();
    assert_eq!(on_disk, listed);
}
