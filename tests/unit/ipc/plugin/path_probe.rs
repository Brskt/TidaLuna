//! Tests for `src/ipc/plugin/path_probe.rs`, attached by `#[path]`.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use super::*;

/// A read stuck on a dead mount is given up on, and while it stays stuck no further read starts:
/// a plugin asking again cannot pile threads up behind the mount.
#[tokio::test]
async fn a_stuck_read_is_given_up_on_and_holds_back_the_next() {
    static BUSY: AtomicBool = AtomicBool::new(false);
    let probe = Probe::new(Duration::from_millis(20), &BUSY);
    let (release, stuck) = std::sync::mpsc::channel::<()>();

    let first = probe.run(move || stuck.recv().is_ok()).await;
    assert_eq!(first, None, "past the deadline");

    let ran = Arc::new(AtomicBool::new(false));
    let marker = ran.clone();
    let second = probe
        .run(move || marker.store(true, Ordering::SeqCst))
        .await;
    assert_eq!(second, None, "the first read is still stuck");
    assert!(!ran.load(Ordering::SeqCst), "no second thread was started");

    release.send(()).unwrap();
    for _ in 0..200 {
        if !probe.busy.load(Ordering::Acquire) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(
        probe.run(|| 7).await,
        Some(7),
        "free again once the mount answers"
    );
}
