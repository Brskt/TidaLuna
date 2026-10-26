//! Tests for the actor in `src/db.rs`, attached by `#[path]`.
//!
//! The first two tests below are what let IPC handlers stop waiting on the actor from the
//! CEF UI thread: order survives the change, and a queued write can still land before exit.

use super::*;
use std::sync::{Arc, Mutex};

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
