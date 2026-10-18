//! Tests for `src/platform/durable_fs.rs`, attached to it by `#[path]`.

use super::*;

#[test]
fn persisting_replaces_the_destination() {
    use std::io::Write;

    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("mark");
    std::fs::write(&dest, b"old").unwrap();
    let mut tmp = tempfile::NamedTempFile::new_in(dir.path()).unwrap();
    tmp.write_all(b"new").unwrap();
    tmp.as_file().sync_all().unwrap();

    persist(tmp, &dest).unwrap();

    assert_eq!(std::fs::read(&dest).unwrap(), b"new");
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[cfg(unix)]
#[test]
fn a_bare_file_name_does_not_fail_to_flush() {
    sync_parent(Path::new("bare-name")).unwrap();
}

#[cfg(target_os = "linux")]
mod syscalls {
    use super::*;
    use crate::platform::syscall_trace::{Event, traced};

    #[test]
    fn persisting_flushes_only_the_directory() {
        use std::io::Write;

        let dir = tempfile::tempdir().unwrap();
        let canonical = dir.path().canonicalize().unwrap();
        let mut tmp = tempfile::NamedTempFile::new_in(dir.path()).unwrap();
        tmp.write_all(b"new").unwrap();
        tmp.as_file().sync_all().unwrap();

        let (persisted, events) = traced(|| persist(tmp, &dir.path().join("mark")));

        persisted.unwrap();
        assert_eq!(
            events,
            vec![Event::Fsync {
                path: canonical,
                directory: true
            }]
        );
    }
}
