use rusqlite::Connection;
use std::path::Path;
use std::sync::mpsc;
use std::thread::ThreadId;

type BoxedFn = Box<dyn FnOnce(&mut Connection, &mut Connection) + Send + 'static>;

/// Handle to the database actor thread.
///
/// All SQLite operations go through [`call`](DbActor::call), which sends a
/// closure to the dedicated thread and blocks until the result is ready.
///
/// `DbActor` is `Clone + Send + Sync` - it only holds a channel sender and
/// the actor thread ID (for re-entrancy detection).
#[derive(Clone)]
pub(crate) struct DbActor {
    tx: mpsc::SyncSender<BoxedFn>,
    actor_thread_id: ThreadId,
}

impl DbActor {
    /// Spawn the database actor thread and open both databases.
    ///
    /// The actor thread owns `plugins.db` and `settings.db` connections.
    /// Returns once both connections are open and schema is initialized.
    pub fn open(data_dir: &Path) -> rusqlite::Result<Self> {
        let (tx, rx) = mpsc::sync_channel::<BoxedFn>(64);
        let (init_tx, init_rx) = mpsc::sync_channel::<Result<ThreadId, rusqlite::Error>>(0);
        let data_dir = data_dir.to_owned();

        std::thread::Builder::new()
            .name("db-actor".into())
            .spawn(move || {
                let thread_id = std::thread::current().id();
                let result = (|| -> rusqlite::Result<(Connection, Connection)> {
                    let mut plugins_conn = Connection::open(data_dir.join("plugins.db"))?;
                    let mut settings_conn = Connection::open(data_dir.join("settings.db"))?;
                    crate::plugins::store::init_schema(&mut plugins_conn)?;
                    crate::settings::init_schema(&mut settings_conn)?;
                    Ok((plugins_conn, settings_conn))
                })();

                let (mut plugins_conn, mut settings_conn) = match result {
                    Ok(conns) => {
                        let _ = init_tx.send(Ok(thread_id));
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

        let actor_thread_id = init_rx.recv().expect("db-actor init channel closed")?;
        Ok(Self {
            tx,
            actor_thread_id,
        })
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
            std::thread::current().id() != self.actor_thread_id,
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
    /// callback from inside the closure. Bounded, so a caller that outruns the actor by 64
    /// operations waits: dropping a write would lose it silently.
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
            std::thread::current().id() != self.actor_thread_id,
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
