use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

const PREFIX: &str = "tidalunar-test-";

/// A directory touched more recently than this may belong to a process that has created it
/// but not locked it yet. Only an optimization; the lock is what protects a live directory.
const GRACE: Duration = Duration::from_secs(10);

/// Held for the process lifetime: the OS releases it at exit, crash included, which is how
/// a sweeper tells a dead owner from a live one.
const ALIVE: &str = ".alive";

pub(crate) struct TestRoot {
    /// Created: a folder grant resolves the home to tell whether a folder holds it.
    pub(crate) home: PathBuf,
    pub(crate) data: PathBuf,
}

pub(crate) fn root() -> &'static TestRoot {
    static ROOT: OnceLock<TestRoot> = OnceLock::new();
    static LOCK: OnceLock<File> = OnceLock::new();
    ROOT.get_or_init(|| {
        // `temp_dir` hands `TMPDIR`/`TMP` back verbatim, relative or not.
        let temp = std::path::absolute(std::env::temp_dir()).expect("an absolute temp dir");
        assert!(temp.to_str().is_some(), "temp dir {temp:?} is not UTF-8");
        let (alive, dir) = claim(&temp);
        LOCK.set(alive).expect("one test root per process");
        sweep(&temp, &dir);
        let home = dir.join("home");
        fs::create_dir(&home).expect("a test home folder");
        TestRoot {
            home,
            data: dir.join("data"),
        }
    })
}

/// Create a root under `temp` with a name no one has had, and lock its `.alive`. Nothing is
/// deleted here. A name keeps one owner, and the sweeper's lock stays tied to it. A taken
/// name, a failed lock or an `.alive` gone once locked starts over, a few times, not forever.
fn claim(temp: &Path) -> (File, PathBuf) {
    for _ in 0..8 {
        let mut nonce = [0u8; 4];
        getrandom::fill(&mut nonce).expect("a random test root name");
        let dir = temp.join(format!(
            "{PREFIX}{}-{:08x}",
            std::process::id(),
            u32::from_le_bytes(nonce)
        ));
        match fs::create_dir(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => panic!("cannot create the test root {dir:?}: {e}"),
        }
        let Ok(alive) = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(dir.join(ALIVE))
        else {
            continue;
        };
        if alive.try_lock().is_ok() && fs::metadata(dir.join(ALIVE)).is_ok() {
            return (alive, dir);
        }
    }
    panic!("could not claim a test root in {temp:?}");
}

/// Remove the test roots in `temp` whose owner is gone: those whose `.alive` locks, or that
/// never got one. The lock is held through the removal, never released in between. A removal
/// that fails is left to the next process's sweep.
fn sweep(temp: &Path, own: &Path) {
    let Ok(entries) = fs::read_dir(temp) else {
        return;
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        if dir == own || !entry.file_name().to_string_lossy().starts_with(PREFIX) {
            continue;
        }
        // An unreadable or future mtime is young: kept until it can be judged.
        let young = fs::metadata(&dir)
            .and_then(|meta| meta.modified())
            .map_or(true, |at| {
                SystemTime::now()
                    .duration_since(at)
                    .map_or(true, |age| age < GRACE)
            });
        if young {
            continue;
        }
        match OpenOptions::new()
            .read(true)
            .write(true)
            .open(dir.join(ALIVE))
        {
            Ok(alive) => {
                if alive.try_lock().is_ok() {
                    let _ = fs::remove_dir_all(&dir);
                }
                // Released only now, after the removal.
                drop(alive);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let _ = fs::remove_dir_all(&dir);
            }
            Err(_) => {}
        }
    }
}
