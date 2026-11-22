//! Tests for `src/platform/sdk_storage.rs`, attached to it by `#[path]`.

use super::*;

/// The purge the page runs, executed on a stand-in `localStorage`: every SDK credential key of
/// TIDAL's origin goes, keys that only look alike stay, and another origin keeps everything.
#[test]
fn the_page_purge_takes_every_credential_key_on_tidals_origin_alone() {
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let name = if cfg!(windows) { "bun.exe" } else { "bun" };
    let bun = crate::native_runtime::find_binary(&root.join("dist"), name)
        .unwrap_or_else(|| std::path::PathBuf::from(name));
    let harness = format!(
        "const purge = new Function('location', 'localStorage', {script});
         const run = (origin) => {{
             const ls = {{'AuthDB/u1Salt': 1, 'AuthDB/tidalData': 1, 'AuthDBx': 1, 'authdb/y': 1, 'ls/a': 1}};
             Object.defineProperty(ls, 'removeItem', {{ value: (k) => delete ls[k] }});
             purge({{ origin }}, ls);
             return Object.keys(ls);
         }};
         console.log(JSON.stringify([run({origin}), run('https://evil.example')]));",
        script = serde_json::Value::from(credentials_purge_script()),
        origin = serde_json::Value::from(ORIGIN),
    );
    let out = std::process::Command::new(&bun)
        .args(["-e", &harness])
        .output()
        .unwrap_or_else(|e| panic!("cannot run {}: {e}", bun.display()));
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let left: Vec<Vec<String>> =
        serde_json::from_slice(&out.stdout).expect("the harness prints JSON");
    assert_eq!(
        left,
        [
            vec!["AuthDBx", "authdb/y", "ls/a"],
            vec![
                "AuthDB/u1Salt",
                "AuthDB/tidalData",
                "AuthDBx",
                "authdb/y",
                "ls/a"
            ],
        ]
    );
}

/// Copies the fixture first: opening a store replays and rewrites some of its files.
fn chrome_multitable_store() -> tempfile::TempDir {
    let fixture = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/unit/platform/sdk_storage/leveldb_chrome_multitable");
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("leveldb");
    std::fs::create_dir(&store).unwrap();
    for entry in std::fs::read_dir(&fixture).unwrap() {
        let entry = entry.unwrap();
        if entry.file_name() != "recipe.md" {
            std::fs::copy(entry.path(), store.join(entry.file_name())).unwrap();
        }
    }
    dir
}

fn read(dir: &tempfile::TempDir, ls_key: &str) -> Option<Vec<u8>> {
    match read_ls_value(&dir.path().join("leveldb"), ls_key) {
        LsValue::Value(bytes) => Some(bytes),
        LsValue::Missing => None,
        LsValue::Corrupt => panic!("{ls_key} read as corrupt"),
        LsValue::Unreadable => panic!("{ls_key}: store read as unreadable"),
    }
}

const UUID: &str = "0f1e2d3c-4b5a-4978-8796-a5b4c3d2e1f0";

fn opened(dir: &tempfile::TempDir) -> Box<CredentialStore> {
    match open_credentials(&dir.path().join("leveldb")) {
        OpenedCredentials::Opened(store) => store,
        OpenedCredentials::Missing => panic!("the store reads as missing"),
        OpenedCredentials::Unreadable => panic!("the store reads as unreadable"),
    }
}

fn ids(store: &CredentialStore) -> Vec<&str> {
    store.blobs().iter().map(|blob| blob.id.as_str()).collect()
}

/// Every id is read off the key names of a store Chromium wrote over several tables, a key it
/// deleted excepted, and every key under the prefix goes in one batch: a lookup by name would miss
/// live keys behind another key's tombstone. Keys that only look alike stay as they were.
#[test]
fn every_credential_key_of_the_origin_goes_and_every_other_key_stays() {
    let dir = chrome_multitable_store();
    let app = format!("{UUID}_clientCredentials_PROD");
    let store = opened(&dir);
    assert_eq!(ids(&store), [UUID, app.as_str(), "tidal", "u1"]);

    assert_eq!(store.purge(|_| true), Purge::Deleted(13));

    for id in [UUID, app.as_str(), "tidal", "u1", "gone"] {
        for suffix in BLOB_SUFFIXES {
            assert_eq!(
                read(&dir, &format!("AuthDB/{id}{suffix}")),
                None,
                "{id}{suffix}"
            );
        }
    }
    assert_eq!(read(&dir, "AuthDBx"), Some(b"neighbour".to_vec()));
    assert_eq!(read(&dir, "authdb/y"), Some(b"neighbour".to_vec()));
    assert_eq!(read(&dir, "ls/a1"), Some(b"va1".to_vec()));
    assert_eq!(read(&dir, "ls/del_rewrite"), Some(b"new".to_vec()));
    assert!(opened(&dir).blobs().is_empty());
}

/// A damaged table panics rusty-leveldb's iterator: the store reads as unreadable and its files
/// are left as they were.
#[test]
fn a_store_with_a_damaged_table_reads_as_unreadable_and_stays_as_it_was() {
    let dir = chrome_multitable_store();
    let table = dir.path().join("leveldb").join("000006.ldb");
    let whole = std::fs::read(&table).unwrap();
    let damaged = &whole[..whole.len() / 2];
    std::fs::write(&table, damaged).unwrap();

    assert!(matches!(
        open_credentials(&dir.path().join("leveldb")),
        OpenedCredentials::Unreadable
    ));
    assert_eq!(std::fs::read(&table).unwrap(), damaged);
}

/// Runs `look` while another process holds the store's `LOCK` as Chromium does: a `fcntl` lock is
/// invisible to the caller's own probe and to `flock`; neither a thread nor rusty-leveldb can
/// stand in.
#[cfg(unix)]
fn while_another_process_locks<R>(path: &std::path::Path, look: impl FnOnce() -> R) -> R {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let lock = CString::new(path.join("LOCK").as_os_str().as_bytes()).unwrap();

    let mut ready: [libc::c_int; 2] = [0; 2];
    // SAFETY: `pipe` fills the two slots of our own array.
    assert_eq!(unsafe { libc::pipe(ready.as_mut_ptr()) }, 0);
    // SAFETY: the child only makes async-signal-safe calls (open, fcntl, write, pause, _exit),
    // on a path and a struct built before the fork.
    let child = unsafe { libc::fork() };
    if child == 0 {
        unsafe {
            let fd = libc::open(lock.as_ptr(), libc::O_RDWR);
            let mut held: libc::flock = std::mem::zeroed();
            held.l_type = libc::F_WRLCK as _;
            held.l_whence = libc::SEEK_SET as _;
            if fd < 0 || libc::fcntl(fd, libc::F_SETLK, &held) != 0 {
                libc::_exit(1);
            }
            libc::write(ready[1], b"x".as_ptr().cast(), 1);
            loop {
                libc::pause();
            }
        }
    }
    assert!(child > 0, "fork failed");

    let mut byte = 0u8;
    // SAFETY: closing our write end makes a child that exits early read as 0 bytes; the read
    // fills our own one-byte buffer.
    let got = unsafe {
        libc::close(ready[1]);
        libc::read(ready[0], std::ptr::from_mut(&mut byte).cast(), 1)
    };
    let seen = look();
    // SAFETY: `child` is our own fork, reaped after the kill; `ready[0]` is ours to close.
    unsafe {
        libc::kill(child, libc::SIGKILL);
        libc::waitpid(child, std::ptr::null_mut(), 0);
        libc::close(ready[0]);
    }

    assert_eq!(got, 1, "the holder could not lock the store");
    seen
}

#[cfg(unix)]
#[test]
fn credentials_another_process_locks_are_neither_read_nor_purged() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("leveldb");
    write_ls_values(&path, &[("AuthDB/tidalData", b"value")]).unwrap();
    let refused = while_another_process_locks(&path, || {
        matches!(open_credentials(&path), OpenedCredentials::Unreadable)
    });
    assert!(refused);
    assert_eq!(read(&dir, "AuthDB/tidalData"), Some(b"value".to_vec()));
}
