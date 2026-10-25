//! Tests for `src/platform/durable_fs.rs`, attached to it by `#[path]`.

use super::*;

#[test]
fn removing_a_file_in_a_directory_that_is_not_there_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    remove_file(&dir.path().join("no-such-dir").join("absent")).unwrap();
}

#[test]
fn a_tombstone_leaves_the_file_empty_and_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("token");
    std::fs::write(&file, b"secret").unwrap();

    tombstone(&file).unwrap();

    assert_eq!(std::fs::metadata(&file).unwrap().len(), 0);
}

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
    fn removal_empties_then_unlinks_then_flushes_the_directory() {
        let dir = tempfile::tempdir().unwrap();
        let canonical = dir.path().canonicalize().unwrap();
        let file = dir.path().join("token");
        std::fs::write(&file, b"secret").unwrap();

        let (removed, events) = traced(|| remove_file(&file));

        removed.unwrap();
        assert!(!file.exists());
        assert_eq!(
            events,
            vec![
                Event::Fsync {
                    path: canonical.join("token"),
                    directory: false
                },
                Event::Unlink(file),
                Event::Fsync {
                    path: canonical,
                    directory: true
                },
            ]
        );
    }

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
