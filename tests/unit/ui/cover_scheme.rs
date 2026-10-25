//! Tests for `src/ui/cover_scheme.rs`, attached by `#[path]`.

use super::*;

/// Every other requester is refused before work starts, a service worker's own fetch (which
/// arrives with no frame) included.
#[test]
fn only_a_tidal_page_may_request_a_cover() {
    for (frame, refused) in [
        (Some("https://desktop.tidal.com/"), false),
        (Some("https://desktop.tidal.com/album/1"), false),
        (Some("https://desktop.tidal.com/login"), false),
        (Some("https://desktop.tidal.com/login/auth"), false),
        (Some(""), true),
        (Some("about:blank"), true),
        (Some("data:text/html;base64,PGgxPmhpPC9oMT4="), true),
        (Some("devtools://devtools/bundled/inspector.html"), true),
        (Some("https://evil.example/"), true),
        (Some("https://desktop.tidal.com.evil.example/"), true),
        (Some("https://listen.tidal.com/"), true),
        (None, true),
    ] {
        assert_eq!(requester_refusal(frame).is_some(), refused, "{frame:?}");
    }
}

/// Long past any answer that is coming: a job that never answers fails its test instead of
/// stalling the run.
const ANSWER_DEADLINE: std::time::Duration = std::time::Duration::from_secs(5);

/// A key of its own per test: `COVER_JOBS` is one table for the whole test binary.
fn job_key(name: &str) -> String {
    format!("test://cover-jobs/{name}")
}

/// A load that panics at its first poll, as the `expect` behind `COVER_CLIENT` would.
async fn panicking_load() -> CoverOutcome<Vec<u8>> {
    panic!("simulated panic in the cover load")
}

/// A load that waits for its test to hand it an outcome.
fn gated_load() -> (
    tokio::sync::oneshot::Sender<CoverOutcome<Vec<u8>>>,
    impl std::future::Future<Output = CoverOutcome<Vec<u8>>>,
) {
    let (tx, rx) = tokio::sync::oneshot::channel();
    (
        tx,
        async move { rx.await.unwrap_or(CoverOutcome::Abandoned) },
    )
}

async fn answer_within_deadline(rx: CoverAnswer) -> Arc<CoverResponse> {
    tokio::time::timeout(ANSWER_DEADLINE, await_answer(rx))
        .await
        .expect("the waiter got no answer")
}

#[tokio::test]
async fn a_request_after_a_job_panicked_starts_a_fresh_one() {
    let key = job_key("panics-then-asks-again");
    let mut job = None;
    let (_rx, _interest) = join_or_start(key.clone(), |key, mine, result| {
        job = Some(tokio::spawn(run_cover_job(
            key,
            mine,
            result,
            panicking_load(),
        )));
    });
    let died = job.expect("the first request starts the job").await;
    assert!(died.is_err_and(|e| e.is_panic()));

    let mut started = false;
    let _ = join_or_start(key, |_, _, _| started = true);
    assert!(started, "a later request joined a job that had died");
}

#[tokio::test]
async fn every_requester_of_one_job_receives_its_one_answer() {
    let key = job_key("shared");
    let (finish, load) = gated_load();
    let (first, _a) = join_or_start(key.clone(), |key, mine, result| {
        tokio::spawn(run_cover_job(key, mine, result, load));
    });
    let (second, _b) = join_or_start(key.clone(), |_, _, _| {
        panic!("a running job is joined, never started twice")
    });
    assert!(finish.send(CoverOutcome::Decoded(vec![7, 7, 7])).is_ok());

    let first = answer_within_deadline(first).await;
    let second = answer_within_deadline(second).await;
    assert_eq!(first.status, 200);
    assert_eq!(*first.body, vec![7, 7, 7]);
    assert!(Arc::ptr_eq(&first, &second), "one answer, shared");

    let mut started = false;
    let _ = join_or_start(key, |_, _, _| started = true);
    assert!(started, "an answered job is retired, not kept as a cache");
}

#[tokio::test]
async fn a_job_nobody_waits_for_is_replaced_and_never_answers_its_successor() {
    let key = job_key("replaced");
    let (finish_old, old_load) = gated_load();
    let mut old_job = None;
    let (old_rx, old_interest) = join_or_start(key.clone(), |key, mine, result| {
        old_job = Some(tokio::spawn(run_cover_job(key, mine, result, old_load)));
    });
    old_interest.release();

    let (finish_new, new_load) = gated_load();
    let mut started = false;
    let (new_rx, new_interest) = join_or_start(key.clone(), |key, mine, result| {
        started = true;
        tokio::spawn(run_cover_job(key, mine, result, new_load));
    });
    assert!(started, "a job at zero interest is replaced, never joined");
    assert!(!Arc::ptr_eq(&old_interest, &new_interest));

    assert!(finish_old.send(CoverOutcome::Decoded(vec![1])).is_ok());
    old_job
        .expect("the first request starts the job")
        .await
        .expect("the replaced job runs to its end");
    assert!(
        new_rx.borrow().is_none(),
        "the replaced job answered its successor"
    );
    assert_eq!(answer_within_deadline(old_rx).await.status, 502);

    assert!(finish_new.send(CoverOutcome::Decoded(vec![2])).is_ok());
    assert_eq!(*answer_within_deadline(new_rx).await.body, vec![2]);
}
