//! Tests for the notice a replaced plugins.db leaves for the page, attached to `src/db.rs` by
//! `#[path]`: where it lives, that it survives a crash, and how it is told with settings.db's.

use super::db_open_tests::overwrite_with_ff;
use super::*;
use crate::plugins::store::{self, PluginsReset};

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
    assert!(!told.contains("plugins"));
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
