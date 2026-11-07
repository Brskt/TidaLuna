//! The checkpoint of the cache index's write-ahead log, run by a thread of its own on a second
//! connection, keeping its cost out of every commit made under the cache lock.

use rusqlite::{Connection, ffi};
use std::ffi::{c_char, c_int, c_void};
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

/// Pages of log a checkpoint is owed at: SQLite's own default for its automatic one.
pub(super) const WAL_PAGES: c_int = 1000;

/// What a reset log file is cut to: a little above the log a checkpoint leaves behind.
const WAL_FILE_LIMIT: i64 = 8 * 1024 * 1024;

#[derive(Default)]
struct Mailbox {
    wake: bool,
    stop: bool,
}

#[derive(Default)]
struct Shared {
    mailbox: Mutex<Mailbox>,
    bell: Condvar,
}

impl Shared {
    fn mailbox(&self) -> std::sync::MutexGuard<'_, Mailbox> {
        self.mailbox.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn ring(&self) {
        self.mailbox().wake = true;
        self.bell.notify_one();
    }
}

/// Runs on the writer's thread and only rings the bell. Always answers OK: an error would surface
/// as the failure of a statement whose commit has happened.
unsafe extern "C" fn on_commit(
    shared: *mut c_void,
    _db: *mut ffi::sqlite3,
    _schema: *const c_char,
    pages: c_int,
) -> c_int {
    if pages >= WAL_PAGES {
        // SAFETY: `shared` is the pointer `Checkpointer::start` registered with this hook, from
        // an `Arc` the registration owns a count of; `stop` unregisters the hook before that
        // count is released, and the hook only runs inside a statement of the connection.
        unsafe { &*shared.cast::<Shared>() }.ring();
    }
    ffi::SQLITE_OK
}

pub(super) struct Checkpointer {
    shared: Arc<Shared>,
    hooked: bool,
    thread: Option<JoinHandle<()>>,
}

impl Checkpointer {
    /// Take the checkpoint off `conn`: a thread with a connection of its own to `db_path`, woken
    /// by `conn`'s commit hook once the log is [`WAL_PAGES`] long. `None` when it cannot start,
    /// and then `conn` keeps SQLite's automatic checkpoint, which only this success turns off.
    pub(super) fn start(conn: &Connection, db_path: &Path) -> Option<Self> {
        let side = Connection::open(db_path)
            .map_err(|e| crate::vprintln!("[CACHE]  No checkpoint connection: {e}"))
            .ok()?;
        let shared = Arc::new(Shared::default());
        let thread = std::thread::Builder::new()
            .name("cache-checkpoint".into())
            .spawn({
                let shared = Arc::clone(&shared);
                move || run(&side, &shared)
            })
            .map_err(|e| crate::vprintln!("[CACHE]  No checkpoint thread: {e}"))
            .ok()?;
        let mut checkpointer = Self {
            shared,
            hooked: false,
            thread: Some(thread),
        };
        let armed = conn
            .pragma_update(None, "wal_autocheckpoint", 0)
            .and_then(|()| conn.pragma_update(None, "journal_size_limit", WAL_FILE_LIMIT));
        if let Err(e) = armed {
            crate::vprintln!("[CACHE]  Checkpoint pragmas refused: {e}");
            checkpointer.stop(Some(conn));
            return None;
        }
        // SAFETY: `conn` stays open for as long as the hook is registered (`stop` takes it down
        // first); the context is the `Shared` of the `Arc` this count is leaked from, handed back
        // by `stop`; the hook only reads it through a shared reference.
        unsafe {
            let context = Arc::into_raw(Arc::clone(&checkpointer.shared)).cast_mut();
            ffi::sqlite3_wal_hook(conn.handle(), Some(on_commit), context.cast());
        }
        checkpointer.hooked = true;
        // The log a previous session left, checkpointed without waiting for the first writes.
        checkpointer.shared.ring();
        Some(checkpointer)
    }

    /// End the hook, then the thread (the caller takes `conn` down after this returns): a
    /// checkpoint cannot be running on a connection that is being closed.
    pub(super) fn stop(&mut self, conn: Option<&Connection>) {
        if self.hooked
            && let Some(conn) = conn
        {
            // SAFETY: the hook registered by `start` is replaced by none, after which no commit
            // runs it; the count `start` leaked is then handed back, once (`hooked` is cleared).
            unsafe {
                ffi::sqlite3_wal_hook(conn.handle(), None, std::ptr::null_mut());
                Arc::decrement_strong_count(Arc::as_ptr(&self.shared));
            }
            self.hooked = false;
        }
        self.shared.mailbox().stop = true;
        self.shared.bell.notify_one();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run(side: &Connection, shared: &Shared) {
    loop {
        {
            let mut mailbox = shared.mailbox();
            while !mailbox.wake && !mailbox.stop {
                mailbox = shared.bell.wait(mailbox).unwrap_or_else(|e| e.into_inner());
            }
            if mailbox.stop {
                return;
            }
            mailbox.wake = false;
        }
        // Copies what it can without waiting for a reader or a writer, and never takes the
        // writer lock: a commit is never held up by it. A log it could not finish is picked up
        // again by the next commit past the threshold.
        if let Err(e) = side.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |_| Ok(())) {
            crate::verr!("[CACHE]  Checkpoint failed: {e}");
        }
    }
}
