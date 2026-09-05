//! Tests for the non-blocking actor submission in `src/db.rs`, attached by `#[path]`.
//!
//! Both properties below are what let IPC handlers stop waiting on the actor from the CEF UI
//! thread: order survives the change, and a queued write can still be made to land before exit.

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
fn post_runs_the_closure_without_waiting_for_it() {
    let (db, _dir) = actor();
    let seen = Arc::new(Mutex::new(false));

    let flag = Arc::clone(&seen);
    db.post(move |_, _| {
        *flag.lock().unwrap() = true;
    });
    db.flush();

    assert!(*seen.lock().unwrap(), "the posted closure never ran");
}

#[test]
fn posts_run_in_submission_order() {
    let (db, _dir) = actor();
    let order = Arc::new(Mutex::new(Vec::new()));

    for i in 0..64 {
        let order = Arc::clone(&order);
        db.post(move |_, _| order.lock().unwrap().push(i));
    }
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

#[test]
fn flush_returns_only_once_the_queue_has_drained() {
    let (db, _dir) = actor();
    let done = Arc::new(Mutex::new(0u32));

    for _ in 0..128 {
        let done = Arc::clone(&done);
        db.post(move |_, _| *done.lock().unwrap() += 1);
    }
    db.flush();

    assert_eq!(
        *done.lock().unwrap(),
        128,
        "flush is what keeps a settings write from dying with the process at exit"
    );
}
