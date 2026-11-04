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

        assert_eq!(attempts.load(Ordering::SeqCst), 1, "one resolve, dial and start");
        assert_eq!(leader.result.is_ok(), outcome.is_ok());
        assert_eq!(follower.result.is_ok(), outcome.is_ok());
        assert_eq!(
            leader.result.as_ref().err().map(ConnectFailure::message),
            follower.result.as_ref().err().map(ConnectFailure::message),
        );
        assert_eq!(leader.announces_failure(), outcome.is_err());
        assert!(!follower.announces_failure(), "the failure is announced once");
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

/// A leader that dies mid-attempt must not strand the device: the request waiting on it gets a
/// failure, and the next one runs its own attempt instead of waiting on a flight nobody finishes.
#[tokio::test]
async fn a_leader_that_dies_leaves_no_request_hanging() {
    const DEADLINE: Duration = Duration::from_secs(5);

    for abort in [false, true] {
        let flights = Arc::new(ConnectFlights::new());
        let (release, released) = oneshot::channel::<()>();

        let leader = tokio::spawn({
            let flights = flights.clone();
            async move {
                run(&flights, "tidal", async {
                    released.await.unwrap();
                    panic!("the leader dies mid-attempt");
                })
                .await
            }
        });
        tokio::task::yield_now().await;
        let follower = tokio::spawn({
            let flights = flights.clone();
            async move { run(&flights, "tidal", std::future::pending()).await }
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

        let next = tokio::time::timeout(DEADLINE, run(&flights, "tidal", async { Ok(true) }))
            .await
            .expect("a later request is not left waiting on the dead flight");
        assert!(next.led() && matches!(next.result, Ok(true)));
    }
}
