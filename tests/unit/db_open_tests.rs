//! Tests for how `DbActor::open` meets a plugins.db it cannot use, attached to `src/db.rs` by
//! `#[path]`. settings.db has its own repair, tested beside it.

use super::*;
use std::io::{Seek, SeekFrom, Write};

fn beside(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

fn seed(conn: &mut Connection) {
    crate::plugins::store::init_schema(conn).unwrap();
    crate::plugins::store::install(conn, "https://x/p", "p", "{}", "code", "h").unwrap();
    crate::plugins::store::storage_set(conn, "https://x/p", "k", "v").unwrap();
}

/// How the last session ended.
#[derive(Clone, Copy, Debug)]
enum Session {
    /// Closed: the file holds everything, no `-wal` remains.
    Closed,
    /// Exited without closing, early on: the `-wal` holds every page and masks damage to the
    /// file.
    WalOnly,
    /// Exited without closing, later: the first writes are folded into the file and only the last
    /// one is in the `-wal`.
    Folded,
}

fn left_by_a_session(dir: &Path, session: Session) -> PathBuf {
    let folded = matches!(session, Session::Folded);
    let live = tempfile::tempdir().unwrap();
    let live_path = live.path().join("plugins.db");
    let mut conn = Connection::open(&live_path).unwrap();
    crate::plugins::store::init_schema(&mut conn).unwrap();
    crate::plugins::store::install(&mut conn, "https://x/p", "p", "{}", "code", "h").unwrap();
    if folded {
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
            .unwrap();
    }
    crate::plugins::store::storage_set(&mut conn, "https://x/p", "k", "v").unwrap();
    let path = dir.join("plugins.db");
    match session {
        Session::Closed => {
            drop(conn);
            assert!(
                !beside(&live_path, "-wal").exists(),
                "closing the last connection folds the -wal into the file and removes it"
            );
            std::fs::copy(&live_path, &path).unwrap();
        }
        Session::WalOnly | Session::Folded => {
            std::fs::copy(&live_path, &path).unwrap();
            std::fs::copy(beside(&live_path, "-wal"), beside(&path, "-wal")).unwrap();
            drop(conn);
        }
    }
    path
}

enum Damage {
    /// Every byte of the file.
    Overwritten,
    /// The schema page, behind an intact header.
    SchemaPage,
    /// Cut right after the header.
    Truncated,
}

/// Every byte of the file at `path` set to 0xFF, its length kept.
pub(super) fn overwrite_with_ff(path: &Path) {
    let mut file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    let len = file.metadata().unwrap().len() as usize;
    file.write_all(&vec![0xFF; len]).unwrap();
}

fn damage(path: &Path, how: &Damage) {
    let mut file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    match how {
        Damage::Overwritten => overwrite_with_ff(path),
        Damage::SchemaPage => {
            file.seek(SeekFrom::Start(100)).unwrap();
            file.write_all(&[0xAB; 3996]).unwrap();
        }
        Damage::Truncated => file.set_len(100).unwrap(),
    }
}

/// The copy the page is told of for plugins.db, read where the notice lives.
fn told_aside(db: &DbActor) -> Option<PathBuf> {
    db.call_plugins(|c| crate::plugins::store::pending_reset(c).unwrap())
        .and_then(|reset| reset.asides.first().map(PathBuf::from))
}

fn companions(path: &Path) -> Vec<String> {
    let prefix = path.file_name().unwrap().to_string_lossy().into_owned();
    let mut names: Vec<String> = std::fs::read_dir(path.parent().unwrap())
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(&prefix))
        .collect();
    names.sort();
    names
}

#[test]
fn a_damaged_plugins_db_is_replaced_by_an_empty_store_and_the_launch_goes_on() {
    for session in [Session::Closed, Session::Folded] {
        for how in [Damage::Overwritten, Damage::SchemaPage, Damage::Truncated] {
            let dir = tempfile::tempdir().unwrap();
            let path = left_by_a_session(dir.path(), session);
            damage(&path, &how);
            let damaged = std::fs::read(&path).unwrap();

            let db =
                DbActor::open(dir.path()).expect("a damaged plugins.db must not stop the launch");

            assert_eq!(
                db.call_plugins(|c| crate::plugins::store::list(c).len()),
                0,
                "{session:?}: the new store starts empty"
            );
            let aside = told_aside(&db).expect("the replacement is reported");
            // The failed open folded the -wal into the file before the copy: what is kept is the
            // damaged file, header included.
            assert_eq!(
                std::fs::read(&aside).unwrap()[..100],
                damaged[..100],
                "{session:?}: the copy is the damaged file"
            );
            db.call_plugins(|c| {
                crate::plugins::store::install(c, "https://x/q", "q", "{}", "code", "h").unwrap();
            });
            drop(db);

            let again = DbActor::open(dir.path()).unwrap();
            assert_eq!(
                told_aside(&again),
                Some(aside),
                "{session:?}: replaced once, not every launch"
            );
            assert_eq!(
                again.call_plugins(|c| crate::plugins::store::list(c).len()),
                1
            );
        }
    }
}

#[test]
fn a_replaced_plugins_db_leaves_no_working_files_and_settings_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let path = left_by_a_session(dir.path(), Session::Folded);
    damage(&path, &Damage::Overwritten);
    let settings = dir.path().join("settings.db");
    drop(crate::settings::open(&settings).unwrap());
    let settings_before = std::fs::read(&settings).unwrap();

    let db = DbActor::open(dir.path()).unwrap();
    let aside = told_aside(&db).unwrap();
    drop(db);

    for name in companions(&path) {
        assert!(
            !name.ends_with(".new") && !name.contains(".new-"),
            "left {name}"
        );
    }
    assert!(aside.exists());
    assert_eq!(std::fs::read(&settings).unwrap(), settings_before);
}

#[test]
fn a_healthy_plugins_db_is_not_touched() {
    let dir = tempfile::tempdir().unwrap();
    let mut conn = Connection::open(dir.path().join("plugins.db")).unwrap();
    seed(&mut conn);
    drop(conn);

    let db = DbActor::open(dir.path()).unwrap();

    assert!(told_aside(&db).is_none());
    assert_eq!(db.call_plugins(|c| crate::plugins::store::list(c).len()), 1);
    assert_eq!(
        companions(&dir.path().join("plugins.db"))
            .iter()
            .filter(|n| n.contains("corrupt"))
            .count(),
        0
    );
}

#[test]
fn a_plugins_db_another_connection_holds_is_never_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("plugins.db");
    let mut conn = Connection::open(&path).unwrap();
    seed(&mut conn);
    drop(conn);
    let holder = Connection::open(&path).unwrap();
    holder
        .pragma_update(None, "locking_mode", "EXCLUSIVE")
        .unwrap();
    holder
        .execute_batch("CREATE TABLE IF NOT EXISTS held (x)")
        .unwrap();

    let err = DbActor::open(dir.path())
        .err()
        .expect("a locked store cannot open");

    assert_eq!(err.fault, Fault::Locked);
    assert_eq!(err.path, path);
    assert!(err.refusal().contains("another program is holding it"));
    drop(holder);
    let mut kept = Connection::open(&path).unwrap();
    assert_eq!(
        crate::plugins::store::list(&mut kept).len(),
        1,
        "a transient fault discards nothing"
    );
    assert!(companions(&path).iter().all(|n| !n.contains("corrupt")));
}

#[cfg(unix)]
#[test]
fn a_data_folder_that_cannot_be_written_is_reported_as_such() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let mut conn = Connection::open(dir.path().join("plugins.db")).unwrap();
    seed(&mut conn);
    drop(conn);
    let mut mode = std::fs::metadata(dir.path()).unwrap().permissions();
    mode.set_mode(0o555);
    std::fs::set_permissions(dir.path(), mode.clone()).unwrap();
    assert!(
        std::fs::File::create(dir.path().join("probe")).is_err(),
        "the folder must refuse a write for this test to mean anything (not as root)"
    );

    let err = DbActor::open(dir.path());

    mode.set_mode(0o755);
    std::fs::set_permissions(dir.path(), mode).unwrap();
    let err = err
        .err()
        .expect("a read-only folder cannot open a WAL store");
    assert_eq!(err.fault, Fault::Unwritable);
    assert!(err.refusal().contains("Make the folder writable"));
}

#[test]
fn a_damaged_file_that_cannot_be_replaced_is_a_refusal_not_a_panic() {
    let dir = tempfile::tempdir().unwrap();
    let path = left_by_a_session(dir.path(), Session::Folded);
    damage(&path, &Damage::Overwritten);
    // A directory where the working file goes: the replacement cannot be written.
    std::fs::create_dir(beside(&path, ".new")).unwrap();

    let err = DbActor::open(dir.path())
        .err()
        .expect("no replacement, no store");

    assert_eq!(err.fault, Fault::Unreplaceable);
    assert!(err.refusal().contains("could not replace it"));
    assert_eq!(
        std::fs::read(&path).unwrap()[..100],
        [0xFF; 100],
        "the damaged file stays"
    );
    assert!(companions(&path).iter().all(|n| !n.contains("corrupt")));
}

#[test]
fn a_damaged_file_another_connection_still_holds_is_not_replaced_under_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("plugins.db");
    let mut holder = Connection::open(&path).unwrap();
    seed(&mut holder);
    holder
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .unwrap();
    crate::plugins::store::storage_set(&mut holder, "https://x/p", "late", "v").unwrap();
    damage(&path, &Damage::SchemaPage);

    let err = DbActor::open(dir.path())
        .err()
        .expect("a held, damaged store is not ours to replace");

    assert_eq!(err.fault, Fault::Unreplaceable, "{err:?}");
    assert!(companions(&path).iter().all(|n| !n.contains("corrupt")));
    assert!(!beside(&path, ".new").exists());
}

#[test]
fn damage_the_wal_still_covers_is_not_a_reason_to_replace() {
    let dir = tempfile::tempdir().unwrap();
    let path = left_by_a_session(dir.path(), Session::WalOnly);
    damage(&path, &Damage::Overwritten);

    let db = DbActor::open(dir.path()).unwrap();

    assert!(told_aside(&db).is_none());
    assert_eq!(db.call_plugins(|c| crate::plugins::store::list(c).len()), 1);
}
