//! Tests for how `DbActor::open` meets a plugins.db it cannot use, attached to `src/db.rs` by
//! `#[path]`. settings.db has its own repair, tested beside it.

use super::*;

fn seed(conn: &mut Connection) {
    crate::plugins::store::init_schema(conn).unwrap();
    crate::plugins::store::install(conn, "https://x/p", "p", "{}", "code", "h").unwrap();
    crate::plugins::store::storage_set(conn, "https://x/p", "k", "v").unwrap();
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
fn a_healthy_plugins_db_is_not_touched() {
    let dir = tempfile::tempdir().unwrap();
    let mut conn = Connection::open(dir.path().join("plugins.db")).unwrap();
    seed(&mut conn);
    drop(conn);

    let db = DbActor::open(dir.path()).unwrap();

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
fn a_fault_with_a_remedy_says_what_to_do_and_an_unknown_one_shows_its_cause() {
    let at = |fault| OpenError {
        path: PathBuf::from("/d/plugins.db"),
        fault,
        detail: "why".into(),
    };
    for (fault, advice) in [
        (Fault::Locked, "Close any other TidaLunar"),
        (Fault::Unwritable, "Make the folder writable"),
        (Fault::Unreplaceable, "Move that file"),
    ] {
        let text = at(fault).refusal();
        assert!(
            text.contains("/d/plugins.db") && text.contains(advice),
            "{text}"
        );
    }
    assert!(at(Fault::Other).refusal().contains("(why)"));
}
