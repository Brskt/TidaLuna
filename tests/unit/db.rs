//! Tests for `src/db.rs`, attached to it by `#[path]`.

use super::*;
use crate::plugins::store::{self, PluginsReset};
use std::io::{Seek, SeekFrom, Write};
use std::sync::{Arc, Mutex};

// The actor. The first two tests below are what let IPC handlers stop waiting on the actor from
// the CEF UI thread: order survives the change, and a queued write can still land before exit.

fn actor() -> (DbActor, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("temp dir");
    let db = DbActor::open(dir.path()).expect("db actor");
    // The directory is returned with the actor: dropping it early would pull the database
    // files out from under the thread.
    (db, dir)
}

#[test]
fn posts_run_in_submission_order() {
    let (db, _dir) = actor();
    let order = Arc::new(Mutex::new(Vec::new()));

    for i in 0..64 {
        let order = Arc::clone(&order);
        db.post(move |_, _| order.lock().unwrap().push(i));
    }
    // Returning only once the queue has drained is what keeps a settings write from dying with
    // the process at exit.
    db.flush();

    let seen = order.lock().unwrap().clone();
    assert_eq!(
        seen,
        (0..64).collect::<Vec<i32>>(),
        "submission order is what a storage set followed by a get relies on"
    );
}

#[test]
fn a_later_call_observes_every_earlier_post() {
    let (db, _dir) = actor();
    let count = Arc::new(Mutex::new(0u32));

    for _ in 0..32 {
        let count = Arc::clone(&count);
        db.post(move |_, _| *count.lock().unwrap() += 1);
    }
    // No flush: the blocking call is queued behind the posts and cannot run before them.
    let observed = {
        let count = Arc::clone(&count);
        db.call_plugins(move |_| *count.lock().unwrap())
    };

    assert_eq!(observed, 32, "a call overtook the posts queued before it");
}

/// One panicking operation must not take the thread with it: every later write would be dropped
/// and every caller left waiting.
#[test]
fn a_panicking_operation_leaves_the_actor_serving() {
    let (db, _dir) = actor();
    db.call_plugins(|pc| {
        pc.execute_batch("CREATE TABLE t(v INTEGER); INSERT INTO t VALUES (1);")
            .unwrap()
    });

    db.post(|pc, _| {
        let _stable = Stable::begin(pc);
        let tx = pc.unchecked_transaction().unwrap();
        tx.execute("UPDATE t SET v = 2", []).unwrap();
        panic!("an operation failing mid-transaction");
    });

    let synchronous: i64 = db.call_plugins(|pc| {
        pc.query_row("PRAGMA synchronous", [], |r| r.get(0))
            .unwrap()
    });
    assert_eq!(synchronous, 1, "the full sync outlived its operation");
}

// How `DbActor::open` meets a plugins.db it cannot use. settings.db has its own repair, tested
// beside it.

fn beside(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

fn plugin(raw: &str) -> crate::plugins::name::PluginName {
    crate::plugins::name::PluginName::parse(raw).unwrap()
}

fn seed(conn: &mut Connection) {
    crate::plugins::store::init_schema(conn).unwrap();
    crate::plugins::store::install(conn, "https://x/p", &plugin("p"), "{}", "code", "h").unwrap();
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
    crate::plugins::store::install(&conn, "https://x/p", &plugin("p"), "{}", "code", "h").unwrap();
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
fn overwrite_with_ff(path: &Path) {
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
                db.call_plugins(|c| crate::plugins::store::list(c).unwrap().len()),
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
                crate::plugins::store::install(c, "https://x/q", &plugin("q"), "{}", "code", "h")
                    .unwrap();
            });
            drop(db);

            let again = DbActor::open(dir.path()).unwrap();
            assert_eq!(
                told_aside(&again),
                Some(aside),
                "{session:?}: replaced once, not every launch"
            );
            assert_eq!(
                again.call_plugins(|c| crate::plugins::store::list(c).unwrap().len()),
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
    assert_eq!(
        db.call_plugins(|c| crate::plugins::store::list(c).unwrap().len()),
        1
    );
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
        crate::plugins::store::list(&mut kept).unwrap().len(),
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
    assert_eq!(
        db.call_plugins(|c| crate::plugins::store::list(c).unwrap().len()),
        1
    );
}

// The notice a replaced plugins.db leaves for the page: where it lives, that it survives a crash,
// and how it is told with settings.db's.

fn damaged_plugins_db(dir: &Path) -> PathBuf {
    let path = dir.join("plugins.db");
    let mut conn = Connection::open(&path).unwrap();
    store::init_schema(&mut conn).unwrap();
    let name = crate::plugins::name::PluginName::parse("p").unwrap();
    store::install(&conn, "https://x/p", &name, "{}", "code", "h").unwrap();
    drop(conn);
    overwrite_with_ff(&path);
    path
}

fn damaged_settings_db(dir: &Path) {
    let path = dir.join("settings.db");
    drop(crate::settings::open(&path).unwrap());
    overwrite_with_ff(&path);
}

fn pending_json(db: &DbActor) -> String {
    db.call(|plugins, settings| serde_json::to_string(&pending_repairs(plugins, settings)).unwrap())
}

fn recorded(path: &Path) -> Vec<String> {
    store::recorded_resets(&Connection::open(path).unwrap()).unwrap()
}

#[test]
fn the_notice_is_in_the_file_the_replacement_renamed_into_place() {
    let dir = tempfile::tempdir().unwrap();
    let path = damaged_plugins_db(dir.path());
    let cause = rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_NOTADB),
        None,
    );

    // The process ends right here: nothing else of the launch ran.
    drop(replace_plugins(&path, &cause).unwrap());

    let named = recorded(&path);
    assert_eq!(named.len(), 1);
    assert!(
        Path::new(&named[0]).exists(),
        "{named:?}: the copy it names exists"
    );
    assert_eq!(
        std::fs::read(&named[0]).unwrap()[..16],
        [0xFF; 16],
        "and it is the damaged file"
    );
}

#[test]
fn the_notice_is_owed_at_every_launch_until_acknowledged_and_then_never_again() {
    let dir = tempfile::tempdir().unwrap();
    let path = damaged_plugins_db(dir.path());
    let first = DbActor::open(dir.path()).unwrap();
    let told = pending_json(&first);
    assert_ne!(told, "null", "the replacement owes a notice");
    drop(first);

    let second = DbActor::open(dir.path()).unwrap();
    assert_eq!(
        pending_json(&second),
        told,
        "owed again, not replaced again"
    );
    assert_eq!(
        recorded(&path).len(),
        1,
        "the second launch made no second copy"
    );
    second.call(|plugins, settings| acknowledge_repairs(plugins, settings));
    drop(second);

    let third = DbActor::open(dir.path()).unwrap();
    assert_eq!(pending_json(&third), "null");
}

#[test]
fn a_plugins_reset_alone_is_told_without_a_settings_part() {
    let dir = tempfile::tempdir().unwrap();
    damaged_plugins_db(dir.path());

    let db = DbActor::open(dir.path()).unwrap();

    let told: serde_json::Value = serde_json::from_str(&pending_json(&db)).unwrap();
    let object = told.as_object().unwrap();
    assert_eq!(object.keys().collect::<Vec<_>>(), ["plugins"]);
    let asides = told["plugins"]["asides"].as_array().unwrap();
    assert_eq!(asides.len(), 1);
    assert!(Path::new(asides[0].as_str().unwrap()).exists());
}

#[test]
fn a_settings_repair_alone_keeps_the_shape_the_page_already_reads() {
    let dir = tempfile::tempdir().unwrap();
    damaged_settings_db(dir.path());

    let db = DbActor::open(dir.path()).unwrap();

    let told = pending_json(&db);
    let settings_only = db.call_settings(|conn| {
        serde_json::to_string(&crate::settings::pending_notice(conn)).unwrap()
    });
    assert_eq!(told, settings_only);
    let value: serde_json::Value = serde_json::from_str(&told).unwrap();
    assert!(value.get("plugins").is_none());
}

#[test]
fn both_repairs_of_one_launch_are_told_together_and_acknowledged_together() {
    let dir = tempfile::tempdir().unwrap();
    damaged_plugins_db(dir.path());
    damaged_settings_db(dir.path());

    let db = DbActor::open(dir.path()).unwrap();

    let told: serde_json::Value = serde_json::from_str(&pending_json(&db)).unwrap();
    assert_eq!(told["trust"], "unknown");
    assert_eq!(told["saved"], true);
    assert_eq!(told["plugins"]["asides"].as_array().unwrap().len(), 1);

    db.call(|plugins, settings| acknowledge_repairs(plugins, settings));
    assert_eq!(pending_json(&db), "null");
    assert_eq!(
        db.call_plugins(|conn| store::recorded_resets(conn).unwrap().len()),
        0
    );
}

#[test]
fn a_second_reset_before_the_first_was_told_names_both_copies() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("plugins.db");
    let mut conn = Connection::open(&path).unwrap();
    store::init_schema(&mut conn).unwrap();
    store::record_reset(&conn, "/kept/plugins.db.corrupt-1-0").unwrap();
    drop(conn);
    let cause = rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CORRUPT),
        None,
    );

    drop(replace_plugins(&path, &cause).unwrap());

    let named = recorded(&path);
    assert_eq!(named.len(), 2);
    assert_eq!(
        named[0], "/kept/plugins.db.corrupt-1-0",
        "the older one first"
    );
    assert!(named[1].contains("plugins.db.corrupt-"), "{named:?}");
}

#[test]
fn a_second_reset_of_a_file_whose_notice_cannot_be_read_names_the_new_copy_only() {
    let dir = tempfile::tempdir().unwrap();
    let path = damaged_plugins_db(dir.path());
    drop(DbActor::open(dir.path()).unwrap());
    overwrite_with_ff(&path);

    drop(DbActor::open(dir.path()).unwrap());

    let named = recorded(&path);
    assert_eq!(named.len(), 1, "{named:?}");
    let copies = std::fs::read_dir(dir.path())
        .unwrap()
        .flatten()
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .contains("plugins.db.corrupt-")
        })
        .count();
    assert_eq!(
        copies, 2,
        "both copies are on disk, only the newer is named"
    );
}

#[test]
fn the_replacement_store_holds_the_notice_before_anything_is_put_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("plugins.db");
    let mut conn = Connection::open(&path).unwrap();
    store::init_schema(&mut conn).unwrap();
    store::record_reset(&conn, "/kept/plugins.db.corrupt-1-0").unwrap();
    drop(conn);
    let aside = dir.path().join("plugins.db.corrupt-2-0");

    let fresh = replacement_store(&path, &aside).unwrap();

    assert_eq!(
        store::recorded_resets(&fresh).unwrap(),
        ["/kept/plugins.db.corrupt-1-0", aside.to_str().unwrap()]
    );
    assert_eq!(
        recorded(&path),
        ["/kept/plugins.db.corrupt-1-0"],
        "the file in place is untouched until the rename"
    );
}

#[test]
fn a_copy_that_is_gone_is_not_named_but_the_reset_is_still_owed() {
    let dir = tempfile::tempdir().unwrap();
    let mut conn = Connection::open(dir.path().join("plugins.db")).unwrap();
    store::init_schema(&mut conn).unwrap();
    store::record_reset(&conn, "/no/such/plugins.db.corrupt-1-0").unwrap();

    assert_eq!(
        store::pending_reset(&conn).unwrap(),
        Some(PluginsReset { asides: Vec::new() })
    );
}
