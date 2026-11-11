//! Tests for `src/settings/swap.rs`, attached to it by `#[path]`: how a rebuilt store takes the
//! damaged one's place, and the copies kept aside.

use super::*;
use crate::settings::catalog::{
    CONNECT_RECEIVER_ALWAYS_ON, Read, VIDEO_CODEC_ENABLED, load_log_level, load_receiver_always_on,
    load_video_codec_enabled, read, save_log_level,
};
use crate::settings::fixture::{
    CONSENTS, PAGE, asides, block, page_image, root_page, ruin_page, seeded,
};
use crate::settings::store::{Opened, Trust, init_schema, is_clean, open, populate};

/// Every file beside `path` that is a leftover of a rebuild: its working name or a companion.
fn leftovers(path: &Path) -> Vec<PathBuf> {
    ["-journal", ".new", ".new-wal", ".new-shm", ".new-journal"]
        .iter()
        .map(|suffix| beside(path, suffix))
        .filter(|p| p.exists())
        .collect()
}

#[test]
fn a_volatile_run_leaves_the_damaged_store_exactly_where_it_was() {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded(dir.path());
    ruin_page(&path, root_page(&path, "settings"));
    block(&beside(&path, ".new"));
    let before = std::fs::read(&path).unwrap();

    let (mut conn, opened) = open(&path).unwrap();

    assert_eq!(
        opened,
        Opened::Volatile {
            turned_off: CONSENTS.to_vec(),
            trust: Trust::Dropped
        }
    );
    assert!(!load_video_codec_enabled(&mut conn));
    assert!(!load_receiver_always_on(&mut conn));
    drop(conn);
    // Nothing moved, nothing copied: the next launch decides from the same file.
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert!(asides(&path).is_empty());
    std::fs::remove_dir_all(beside(&path, ".new")).unwrap();
    let (mut conn, opened) = open(&path).unwrap();
    assert_eq!(
        opened,
        Opened::Rebuilt {
            turned_off: CONSENTS.to_vec(),
            trust: Trust::Dropped
        }
    );
    assert!(!load_video_codec_enabled(&mut conn));
}

#[test]
fn a_store_still_held_by_another_connection_is_not_replaced_under_its_wal() {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded(dir.path());
    // A session that is still there: its writes sit in the -wal, which the closing connection of
    // `open` cannot fold into the file.
    let mut holder = Connection::open(&path).unwrap();
    init_schema(&mut holder).unwrap();
    save_log_level(&mut holder, 3);
    ruin_page(&path, root_page(&path, "native_trust"));
    assert!(std::fs::metadata(beside(&path, "-wal")).unwrap().len() > 0);
    let before = std::fs::read(&path).unwrap();

    let (mut conn, opened) = open(&path).unwrap();

    assert_eq!(
        opened,
        Opened::Volatile {
            turned_off: vec![],
            trust: Trust::Unknown
        }
    );
    assert_eq!(load_log_level(&mut conn), 3);
    assert!(!load_video_codec_enabled(&mut conn));
    drop(conn);
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert!(asides(&path).is_empty());
    assert!(leftovers(&path).is_empty());
    drop(holder);
    let (_, opened) = open(&path).unwrap();
    assert_eq!(
        opened,
        Opened::Rebuilt {
            turned_off: vec![],
            trust: Trust::Unknown
        }
    );
}

#[test]
fn a_rebuild_that_died_before_the_swap_is_redone_from_the_damaged_store() {
    // The disk a crash leaves: the damaged store, and a working file the dead run never finished
    // (garbage, a complete store with nothing of the refusals, or one written after the copy
    // aside and never renamed over the damaged store).
    let empty_store = {
        let other = tempfile::tempdir().unwrap();
        let mut fresh = Connection::open(other.path().join("x.db")).unwrap();
        init_schema(&mut fresh).unwrap();
        drop(fresh);
        std::fs::read(other.path().join("x.db")).unwrap()
    };
    for (stale, copied_aside) in [
        (b"half a write".to_vec(), false),
        (empty_store, false),
        (b"complete, but not renamed".to_vec(), true),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = seeded(dir.path());
        ruin_page(&path, root_page(&path, "settings"));
        if copied_aside {
            std::fs::copy(&path, beside(&path, ".corrupt-1-0")).unwrap();
        }
        std::fs::write(beside(&path, ".new"), &stale).unwrap();
        std::fs::write(beside(&path, ".new-wal"), b"stale wal").unwrap();

        let (mut conn, opened) = open(&path).unwrap();

        assert_eq!(
            opened,
            Opened::Rebuilt {
                turned_off: CONSENTS.to_vec(),
                trust: Trust::Dropped
            }
        );
        assert!(!load_video_codec_enabled(&mut conn));
        assert!(!load_receiver_always_on(&mut conn));
        assert!(leftovers(&path).is_empty());
    }
}

#[test]
fn the_new_store_is_never_at_the_path_without_its_refusals() {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded(dir.path());
    ruin_page(&path, root_page(&path, "settings"));
    let mut fresh = Connection::open_in_memory().unwrap();
    populate(&mut fresh, &[], true).unwrap();

    replace(&path, &fresh).unwrap();

    // What the file holds the instant it is at the path: schema and refusals, no -wal needed.
    assert!(!beside(&path, ".new").exists());
    assert!(!beside(&path, "-wal").exists());
    let conn = Connection::open(&path).unwrap();
    assert!(is_clean(&conn).unwrap());
    assert!(matches!(
        read::<bool>(&conn, VIDEO_CODEC_ENABLED.key),
        Read::Stored(false)
    ));
    assert!(matches!(
        read::<bool>(&conn, CONNECT_RECEIVER_ALWAYS_ON.key),
        Read::Stored(false)
    ));
}

#[test]
fn a_damaged_store_is_copied_aside_byte_for_byte_with_no_companions() {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded(dir.path());
    let table = root_page(&path, "settings");
    ruin_page(&path, table);
    let damaged = std::fs::read(&path).unwrap();

    open(&path).unwrap();

    let [aside] = asides(&path).try_into().unwrap();
    assert_eq!(std::fs::read(&aside).unwrap(), damaged);
    assert_eq!(page_image(&aside, table), vec![0xAB; PAGE as usize]);
    assert!(!beside(&aside, "-wal").exists());
    assert!(!beside(&aside, "-shm").exists());
}

#[test]
fn the_copies_of_damaged_stores_are_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded(dir.path());
    for _ in 0..(KEPT_ASIDES + 3) {
        ruin_page(&path, root_page(&path, "settings"));
        drop(open(&path).unwrap());
    }
    let before = asides(&path);
    ruin_page(&path, root_page(&path, "settings"));
    drop(open(&path).unwrap());

    let after = asides(&path);
    assert_eq!(after.len(), KEPT_ASIDES);
    assert!(
        after.iter().any(|aside| !before.contains(aside)),
        "the copy just made was the one dropped, not the oldest"
    );
}

/// A damaged store in a directory named `name`: rebuilt on disk, the refusals kept, a copy aside.
fn rebuilt_in_a_directory_named(name: &std::ffi::OsStr) {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join(name);
    std::fs::create_dir(&dir).unwrap();
    let path = seeded(&dir);
    ruin_page(&path, root_page(&path, "settings"));

    let (mut conn, opened) = open(&path).unwrap();

    assert_eq!(
        opened,
        Opened::Rebuilt {
            turned_off: CONSENTS.to_vec(),
            trust: Trust::Dropped
        }
    );
    assert_eq!(asides(&path).len(), 1);
    assert!(leftovers(&path).is_empty());
    assert!(!load_video_codec_enabled(&mut conn));
    assert!(!load_receiver_always_on(&mut conn));
    drop(conn);
    assert_eq!(open(&path).unwrap().1, Opened::Healthy);
}

/// Names a directory can hold on every OS, among them the characters a SQLite URI reads as syntax
/// (`#` starts a fragment, `%` an escape).
#[test]
fn a_directory_name_that_is_special_to_a_uri_does_not_change_where_the_store_goes() {
    for name in ["a b", "a#b", "a%41b", "100%", "\u{e9}t\u{e9}"] {
        rebuilt_in_a_directory_named(std::ffi::OsStr::new(name));
    }
}

/// The rest of the URI syntax (`?` starts the query, `file:` is the scheme) is a valid directory
/// name on Unix only: Windows refuses both characters, and no such directory can exist there.
#[cfg(unix)]
#[test]
fn a_directory_name_that_is_a_uri_query_or_scheme_does_not_change_where_the_store_goes() {
    for name in ["a?b=c", "file:x", "x?mode=ro&nolock=1"] {
        rebuilt_in_a_directory_named(std::ffi::OsStr::new(name));
    }
}

/// Linux only: APFS refuses to create a name that is not UTF-8 (EILSEQ) and exFAT does too, while
/// HFS+ and FAT store it as `%XX`; on macOS the directory built here is not the one named.
#[cfg(target_os = "linux")]
#[test]
fn a_damaged_store_in_a_directory_that_is_not_utf8_is_rebuilt_on_disk() {
    use std::os::unix::ffi::OsStrExt;
    for name in [&b"d\xff"[..], b"\xe9t\xe9", b"d\xff?#%41 x"] {
        rebuilt_in_a_directory_named(std::ffi::OsStr::from_bytes(name));
    }
}
