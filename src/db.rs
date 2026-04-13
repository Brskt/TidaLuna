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
    fn call<F, R>(&self, f: F) -> R
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
