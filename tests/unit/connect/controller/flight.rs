//! Tests for `src/connect/controller/flight.rs`, attached to it by `#[path]`.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::sync::oneshot;

use super::*;

fn outcomes() -> [ConnectResult; 2] {
    [Ok(true), Err(ConnectFailure::Dial("refused".into()))]
}

/// A request whose dial ends as `dial` does and whose settle step installs nothing.
fn run<'a>(
    flights: &'a ConnectFlights,
    fullname: &'a str,
    dial: impl Future<Output = ConnectResult> + 'a,
) -> impl Future<Output = Settled> + 'a {
    run_counting(flights, fullname, dial, Arc::default())
}

/// As `run`, counting the times the request's abort hook fires.
fn run_counting<'a>(
    flights: &'a ConnectFlights,
    fullname: &'a str,
    dial: impl Future<Output = ConnectResult> + 'a,
    aborts: Arc<AtomicUsize>,
) -> impl Future<Output = Settled> + 'a {
    flights.run(
        fullname,
        dial,
        |dialled| match dialled {
            Ok(joined) => Verdict::done(joined),
            Err(failure) => Verdict {
                announce: failure.is_announced(),
                result: Err(failure),
            },
        },
        drop,
        move |_led| {
            aborts.fetch_add(1, Ordering::SeqCst);
        },
    )
}

/// TIDAL resumes at saga start and on every LOGIN_SUCCESS: two requests for one device overlap.
/// They have to share one attempt and one answer, and TIDAL hears of a failure once.
#[tokio::test]
async fn overlapping_requests_for_one_device_share_one_attempt() {
    for outcome in outcomes() {
        let flights = ConnectFlights::new();
        let attempts = AtomicUsize::new(0);
        let (release, released) = oneshot::channel::<()>();

        let leader = run(&flights, "tidal", async {
            attempts.fetch_add(1, Ordering::SeqCst);
            released.await.unwrap();
            outcome.clone()
        });
        let follower = run(&flights, "tidal", async {
            attempts.fetch_add(1, Ordering::SeqCst);
            outcome.clone()
        });
        let (leader, follower, ()) =
            tokio::join!(leader, follower, async { release.send(()).unwrap() });

        assert_eq!(
            attempts.load(Ordering::SeqCst),
            1,
            "one resolve, dial and start"
        );
        assert_eq!(leader.result.is_ok(), outcome.is_ok());
        assert_eq!(follower.result.is_ok(), outcome.is_ok());
        assert_eq!(
            leader.result.as_ref().err().map(ConnectFailure::message),
            follower.result.as_ref().err().map(ConnectFailure::message),
        );
        assert_eq!(leader.announces_failure(), outcome.is_err());
        assert!(
            !follower.announces_failure(),
            "the failure is announced once"
        );
    }
}

/// The user picks device B while device A is still connecting. The newer pick wins: A's callers
/// fail without telling TIDAL, whose CONNECT_TO_DEVICE_FAILED would clear the pick B now owns.
#[tokio::test]
async fn a_request_for_another_device_replaces_the_one_in_flight() {
    let flights = ConnectFlights::new();

    let older = run(&flights, "a", std::future::pending());
    let older_follower = run(&flights, "a", std::future::pending());
    let newer = run(&flights, "b", async { Ok(false) });
    let (older, older_follower, newer) = tokio::join!(older, older_follower, newer);

    for superseded in [&older, &older_follower] {
        assert!(matches!(superseded.result, Err(ConnectFailure::Superseded)));
        assert!(!superseded.announces_failure());
    }
    assert!(matches!(newer.result, Ok(false)));
}

/// A disconnect during a connect: whatever the leader dials after it must not be installed, which
/// would start a session the user just ended, and the requests waiting on it hear it was cancelled.
#[tokio::test]
async fn a_disconnect_cancels_the_attempt_in_flight() {
    let flights = ConnectFlights::new();
    let (installed, discarded) = (AtomicUsize::new(0), AtomicUsize::new(0));
    let (release, released) = oneshot::channel::<()>();

    let leader = flights.run(
        "tidal",
        async {
            released.await.unwrap();
            // The dial completes as the disconnect lands.
            flights.cancel();
            Ok(true)
        },
        |dialled| {
            installed.fetch_add(1, Ordering::SeqCst);
            Verdict::done(dialled.is_ok())
        },
        |_| {
            discarded.fetch_add(1, Ordering::SeqCst);
        },
        |_| {},
    );
    let follower = run(&flights, "tidal", std::future::pending());
    let (leader, follower, ()) =
        tokio::join!(leader, follower, async { release.send(()).unwrap() });

    assert_eq!(installed.load(Ordering::SeqCst), 0);
    assert_eq!(
        discarded.load(Ordering::SeqCst),
        1,
        "what it dialled is closed"
    );
    for cancelled in [&leader, &follower] {
        assert!(matches!(cancelled.result, Err(ConnectFailure::Cancelled)));
        assert!(
            !cancelled.announces_failure(),
            "the disconnect was announced already"
        );
    }
}

/// A leader that dies mid-attempt must not strand the device: the request waiting on it gets a
/// failure, the next one runs its own attempt, and the leader's own query is answered once.
#[tokio::test]
async fn a_leader_that_dies_leaves_no_request_hanging() {
    const DEADLINE: Duration = Duration::from_secs(5);

    for abort in [false, true] {
        let flights = Arc::new(ConnectFlights::new());
        let aborts = Arc::new(AtomicUsize::new(0));
        let (release, released) = oneshot::channel::<()>();

        let leader = tokio::spawn({
            let (flights, aborts) = (flights.clone(), aborts.clone());
            async move {
                run_counting(
                    &flights,
                    "tidal",
                    async {
                        released.await.unwrap();
                        panic!("the leader dies mid-attempt");
                    },
                    aborts,
                )
                .await
            }
        });
        tokio::task::yield_now().await;
        let follower = tokio::spawn({
            let (flights, aborts) = (flights.clone(), aborts.clone());
            async move { run_counting(&flights, "tidal", std::future::pending(), aborts).await }
        });
        tokio::task::yield_now().await;

        if abort {
            leader.abort();
        } else {
            release.send(()).unwrap();
        }
        assert!(leader.await.is_err(), "the leader did not finish");

        let follower = tokio::time::timeout(DEADLINE, follower)
            .await
            .expect("the follower of a dead leader is answered")
            .unwrap();
        assert!(matches!(follower.result, Err(ConnectFailure::Aborted)));
        assert_eq!(
            aborts.load(Ordering::SeqCst),
            1,
            "the dead leader's query is answered"
        );

        let mut next = tokio::time::timeout(
            DEADLINE,
            run_counting(&flights, "tidal", async { Ok(true) }, aborts.clone()),
        )
        .await
        .expect("a later request is not left waiting on the dead flight");
        assert!(next.led() && matches!(next.result, Ok(true)));
        assert!(next.claim());
        drop(next);
        assert_eq!(
            aborts.load(Ordering::SeqCst),
            1,
            "an answered leader is not answered twice"
        );
    }
}

/// At exit the runtime drops the pending requests after CEF is gone, and an abort hook that runs
/// then posts to it. `close` has to answer each one first, once and with its role, and leave the
/// later drop nothing to run.
#[tokio::test]
async fn closing_answers_every_pending_request_once_and_leaves_no_hook() {
    let flights = Arc::new(ConnectFlights::new());
    let answered: Arc<std::sync::Mutex<Vec<bool>>> = Arc::default();
    let request = || {
        let (flights, answered) = (flights.clone(), answered.clone());
        tokio::spawn(async move {
            flights
                .run(
                    "tidal",
                    std::future::pending::<ConnectResult>(),
                    |_| Verdict::done(true),
                    drop,
                    move |led| answered.lock().unwrap().push(led),
                )
                .await
        })
    };
    let leader = request();
    tokio::task::yield_now().await;
    let follower = request();
    tokio::task::yield_now().await;

    flights.close();
    let mut roles = answered.lock().unwrap().clone();
    roles.sort();
    assert_eq!(
        roles,
        [false, true],
        "the leader and its follower, answered by the exit"
    );

    for task in [leader, follower] {
        let mut settled = task.await.unwrap();
        assert!(!settled.claim(), "the caller finds its request answered");
    }
    assert_eq!(
        answered.lock().unwrap().len(),
        2,
        "dropping them runs nothing more"
    );

    let mut late = flights
        .run(
            "tidal",
            std::future::pending::<ConnectResult>(),
            |_| Verdict::done(true),
            drop,
            |_| {},
        )
        .await;
    assert!(matches!(late.result, Err(ConnectFailure::Aborted)));
    assert!(late.claim(), "a request after the exit answers itself");
}
