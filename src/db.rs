use rusqlite::Connection;
use std::path::Path;
use std::sync::{Arc, mpsc};
use std::thread::{JoinHandle, ThreadId};

type BoxedFn = Box<dyn FnOnce(&mut Connection, &mut Connection) + Send + 'static>;

/// Operations the actor's channel holds before a sender waits.
pub(crate) const QUEUE_CAPACITY: usize = 64;

/// What every commit of a store syncs, unless a write asks for more: `NORMAL` leaves the last
/// commits to the OS write-back, which is enough for a window position or a plugin's own key.
pub(crate) const ROUTINE_SYNC: &str = "NORMAL";

/// The connection syncs each commit to the disk while this lives, then goes back to
/// `ROUTINE_SYNC`. Held across a write whose loss would undo a refusal: a setting that grants
/// when absent, a revoked grant, a plugin the user turned off or removed. A store that skips it
/// reads the refusal as consent again after a power cut. `fullfsync` makes the sync reach the
/// platter on macOS, where `fsync` stops at the drive cache; other platforms ignore it.
pub(crate) struct Stable<'c> {
    conn: &'c Connection,
    applied: bool,
}

impl<'c> Stable<'c> {
    /// A connection inside a transaction refuses the change: the write that follows still happens,
    /// and the log says it is not synced. Begin before the transaction, not inside it.
    pub(crate) fn begin(conn: &'c Connection) -> Self {
        if let Err(e) = conn.pragma_update(None, "synchronous", "FULL") {
            crate::verr!(
                "[DB]     {}: cannot sync the next write, a power cut may undo it: {e}",
                store_of(conn)
            );
            return Self {
                conn,
                applied: false,
            };
        }
        if let Err(e) = conn.pragma_update(None, "fullfsync", "ON") {
            crate::verr!(
                "[DB]     {}: cannot reach the platter with the next write: {e}",
                store_of(conn)
            );
        }
        Self {
            conn,
            applied: true,
        }
    }
}

impl Drop for Stable<'_> {
    fn drop(&mut self) {
        if !self.applied {
            return;
        }
        for (pragma, value) in [("fullfsync", "OFF"), ("synchronous", ROUTINE_SYNC)] {
            if let Err(e) = self.conn.pragma_update(None, pragma, value) {
                crate::verr!(
                    "[DB]     {}: cannot restore {pragma}: {e}",
                    store_of(self.conn)
                );
            }
        }
    }
}

/// The file a connection writes, for a log line: the guard serves every store.
fn store_of(conn: &Connection) -> String {
    conn.path()
        .and_then(|path| Path::new(path).file_name())
        .map_or_else(|| "in-memory".into(), |n| n.to_string_lossy().into_owned())
}

/// Handle to the database actor thread.
///
/// All SQLite operations go through [`call`](DbActor::call), which sends a
/// closure to the dedicated thread and blocks until the result is ready.
///
/// `DbActor` is `Clone + Send + Sync`: a channel sender, and the actor thread every clone shares.
#[derive(Clone)]
pub(crate) struct DbActor {
    tx: mpsc::SyncSender<BoxedFn>,
    // Declared after `tx`, which drops first: the join would otherwise wait on a loop this
    // sender keeps open.
    actor: Arc<ActorThread>,
}

/// The actor thread, joined when the last handle drops. Dropping the last one then means both
/// connections are closed: the last close of a WAL database holds an exclusive lock while it
/// checkpoints, and a reopen of the same files during that cleanup fails with `SQLITE_BUSY`
/// (https://www.sqlite.org/wal.html, "Sometimes Queries Return SQLITE_BUSY In WAL Mode").
struct ActorThread {
    /// For re-entrancy detection: a call from this thread would wait on its own reply.
    id: ThreadId,
    handle: Option<JoinHandle<()>>,
}

impl Drop for ActorThread {
    fn drop(&mut self) {
        // A closure on the actor thread may hold the last clone; joining there waits on itself.
        if std::thread::current().id() == self.id {
            return;
        }
        if let Some(handle) = self.handle.take() {
            // A panicking closure already reported itself through the panic hook.
            let _ = handle.join();
        }
    }
}

impl DbActor {
    /// Spawn the database actor thread and open both databases.
    ///
    /// The actor thread owns `plugins.db` and `settings.db` connections.
    /// Returns once both connections are open and schema is initialized.
    pub fn open(data_dir: &Path) -> rusqlite::Result<Self> {
        let (tx, rx) = mpsc::sync_channel::<BoxedFn>(QUEUE_CAPACITY);
        let (init_tx, init_rx) = mpsc::sync_channel::<rusqlite::Result<()>>(0);
        let data_dir = data_dir.to_owned();

        let handle = std::thread::Builder::new()
            .name("db-actor".into())
            .spawn(move || {
                let result = (|| -> rusqlite::Result<(Connection, Connection)> {
                    let mut plugins_conn = Connection::open(data_dir.join("plugins.db"))?;
                    let mut settings_conn = Connection::open(data_dir.join("settings.db"))?;
                    crate::plugins::store::init_schema(&mut plugins_conn)?;
                    crate::settings::init_schema(&mut settings_conn)?;
                    Ok((plugins_conn, settings_conn))
                })();

                let (mut plugins_conn, mut settings_conn) = match result {
                    Ok(conns) => {
                        let _ = init_tx.send(Ok(()));
                        conns
                    }
                    Err(e) => {
                        let _ = init_tx.send(Err(e));
                        return;
                    }
                };

                while let Ok(f) = rx.recv() {
                    f(&mut plugins_conn, &mut settings_conn);
                }
            })
            .expect("failed to spawn db-actor thread");

        // A failed open joins too: its half-open connections close before the error returns.
        let actor = Arc::new(ActorThread {
            id: handle.thread().id(),
            handle: Some(handle),
        });
        init_rx.recv().expect("db-actor init channel closed")?;
        Ok(Self { tx, actor })
    }

    /// Execute a closure on the database actor thread with both connections.
    ///
    /// Prefer [`call_plugins`] or [`call_settings`] for single-DB operations.
    ///
    /// # Panics
    ///
    /// Panics if called from the db-actor thread itself (re-entrancy would
    /// deadlock: the thread blocks waiting for its own response).
    pub fn call<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut Connection, &mut Connection) -> R + Send + 'static,
        R: Send + 'static,
    {
        assert!(
            std::thread::current().id() != self.actor.id,
            "DbActor::call() invoked from the db-actor thread - this would deadlock"
        );
        let (resp_tx, resp_rx) = mpsc::sync_channel::<R>(0);
        self.tx
            .send(Box::new(move |pc, sc| {
                let _ = resp_tx.send(f(pc, sc));
            }))
            .expect("db-actor thread is dead");
        resp_rx.recv().expect("db-actor thread is dead")
    }

    /// Queue a closure on the actor thread without waiting for its result.
    ///
    /// Submission order is execution order: one bounded channel feeds one thread. That is what a
    /// caller on the CEF UI thread needs, where [`call`](DbActor::call) freezes rendering and
    /// input for the whole round trip; a handler owing the renderer an answer invokes its IPC
    /// callback from inside the closure. Bounded, so a caller that outruns the actor by
    /// [`QUEUE_CAPACITY`] operations waits: dropping a write would lose it silently.
    pub fn post<F>(&self, f: F)
    where
        F: FnOnce(&mut Connection, &mut Connection) + Send + 'static,
    {
        if self.tx.send(Box::new(f)).is_err() {
            // Nothing awaits a post; this log is the only place the loss can surface.
            crate::verr!("[DB]     Queued operation dropped: the db-actor thread is dead");
        }
    }

    /// Block until every operation queued before this call has finished.
    ///
    /// The actor drains in order: a zero-capacity rendezvous queued behind them answers
    /// only once they are done. Quit paths need it: [`post`](DbActor::post) lets the caller
    /// move on before the write reaches disk, and process exit drops whatever is still queued.
    ///
    /// # Panics
    ///
    /// Panics if called from the db-actor thread itself, which would wait on its own reply.
    pub fn flush(&self) {
        assert!(
            std::thread::current().id() != self.actor.id,
            "DbActor::flush() invoked from the db-actor thread - this would deadlock"
        );
        let (done_tx, done_rx) = mpsc::sync_channel::<()>(0);
        if self
            .tx
            .send(Box::new(move |_, _| {
                let _ = done_tx.send(());
            }))
            .is_ok()
        {
            let _ = done_rx.recv();
        }
    }

    /// Execute a closure with the `plugins.db` connection.
    pub fn call_plugins<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut Connection) -> R + Send + 'static,
        R: Send + 'static,
    {
        self.call(move |pc, _| f(pc))
    }

    /// Execute a closure with the `settings.db` connection.
    pub fn call_settings<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut Connection) -> R + Send + 'static,
        R: Send + 'static,
    {
        self.call(move |_, sc| f(sc))
    }
}

#[cfg(test)]
#[path = "../tests/unit/db_tests.rs"]
mod db_tests;
