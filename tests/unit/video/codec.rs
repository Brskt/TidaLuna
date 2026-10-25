//! Tests for `src/video/codec.rs`, attached by `#[path]`.

use super::*;

#[test]
fn each_target_url_matches_its_own_platform_shape() {
    let by_file = |name: &str| TARGETS.iter().find(|t| t.file_name == name).unwrap();

    assert_eq!(
        by_file("openh264-2.6.0-win64.dll").url(),
        "https://ciscobinary.openh264.org/openh264-2.6.0-win64.dll.bz2"
    );
    assert_eq!(
        by_file("libopenh264-2.6.0-mac-arm64.dylib").url(),
        "https://ciscobinary.openh264.org/libopenh264-2.6.0-mac-arm64.dylib.bz2"
    );
    assert_eq!(
        by_file("libopenh264-2.6.0-linux64.8.so").url(),
        "https://ciscobinary.openh264.org/libopenh264-2.6.0-linux64.8.so.bz2"
    );
}

#[test]
fn a_host_other_than_the_codec_host_is_refused() {
    for host in [
        "ciscobinary.openh264.org.evil.com",
        "evil-ciscobinary.openh264.org",
        "openh264.org",
        "sub.ciscobinary.openh264.org",
        "audio.tidal.com",
        "resources.tidal.com",
        "desktop.tidal.com",
    ] {
        assert!(!is_codec_host(host), "{host}");
    }
}

/// The host is taken from the parsed url: `https://ciscobinary.openh264.org@evil.com/` reads
/// as `evil.com`. Plaintext is refused outright.
#[test]
fn a_userinfo_prefix_does_not_impersonate_the_codec_host() {
    assert!(!codec_url_allowed(
        "https://ciscobinary.openh264.org@evil.com/x.bz2"
    ));
    assert!(!codec_url_allowed("http://ciscobinary.openh264.org/x.bz2"));
    assert!(!codec_url_allowed("not a url"));
    assert!(codec_url_allowed("https://ciscobinary.openh264.org/x.bz2"));
}

#[test]
fn the_codec_lives_in_its_own_directory_under_the_data_dir() {
    let dir = codec_dir();
    assert!(dir.ends_with("codec"), "{}", dir.display());
    assert!(
        dir.starts_with(crate::state::cache_data_dir()),
        "{} is not under the data dir",
        dir.display()
    );
}

#[test]
fn the_installed_path_is_the_targets_file_name_in_that_directory() {
    let target = current_target().unwrap();
    let path = installed_path(target);
    assert_eq!(path.file_name().unwrap(), target.file_name);
    assert_eq!(path.parent().unwrap(), codec_dir());
}

#[test]
fn decompression_refuses_to_exceed_the_ceiling() {
    use std::io::Write;
    let plain = vec![0u8; 4 * 1024 * 1024];
    let mut encoder = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::default());
    encoder.write_all(&plain).unwrap();
    let packed = encoder.finish().unwrap();

    let err = decompress_capped(&packed, 1024).unwrap_err();
    assert!(err.to_string().contains("ceiling"), "{err}");
}

const CLASSES: [FailureClass; 6] = [
    FailureClass::Network,
    FailureClass::Refused,
    FailureClass::Corrupt,
    FailureClass::Disk,
    FailureClass::Unloadable,
    FailureClass::Crashed,
];

fn failed(class: FailureClass) -> NoDecoder {
    NoDecoder::Unavailable(Unavailable::Failed(class))
}

/// `Starting` and `Downloading` outlive a step only when the supervisor is gone.
#[test]
fn only_a_ready_phase_answers_with_a_path() {
    let path = PathBuf::from("codec");
    assert_eq!(answer(&Phase::Ready(path.clone())), Ok(path));
    assert_eq!(answer(&Phase::Starting), Err(NoDecoder::Stopped));
    assert_eq!(answer(&Phase::Downloading), Err(NoDecoder::Stopped));
    for class in CLASSES {
        let phase = Phase::Unavailable(Unavailable::Failed(class));
        assert_eq!(answer(&phase), Err(failed(class)));
    }
}

/// A file that is no shared library is refused, by its digest or by the loader. Reaching the
/// probe needs a genuine Cisco digest on a file without Wels symbols, a manual-validation item.
#[test]
fn a_file_that_is_not_a_library_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("not-a-codec.so");
    std::fs::write(&path, b"this is not a shared library").unwrap();

    assert!(load_api(&path).is_err());
}

#[test]
fn a_file_whose_digest_does_not_match_the_target_is_removed_and_left_to_download() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wrong-bytes.so");
    std::fs::write(&path, b"these are not the bytes the digest was taken over").unwrap();
    let target = CodecTarget {
        os: "test",
        arch: "test",
        sha256: "not-a-digest-that-will-ever-match",
        file_name: "wrong-bytes.so",
    };

    let status = verify_installed(&path, &target);

    assert!(matches!(status, Verified::Removed));
    assert!(
        !path.exists(),
        "the mismatched file should have been deleted"
    );
}

/// A failing removal stands in for a library Windows keeps loaded: the file reads, and removing
/// it does not succeed.
#[test]
fn a_mismatched_file_that_cannot_be_removed_is_stuck() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("stuck-bytes.so");
    std::fs::write(&path, b"these are not the bytes the digest was taken over").unwrap();
    let target = CodecTarget {
        os: "test",
        arch: "test",
        sha256: "not-a-digest-that-will-ever-match",
        file_name: "stuck-bytes.so",
    };

    let status = verify_installed_with(&path, &target, |_| {
        Err(std::io::ErrorKind::PermissionDenied.into())
    });

    assert!(matches!(status, Verified::Stuck(_)));
    assert!(path.exists(), "the removal was refused");
}

#[test]
fn a_file_that_cannot_be_read_is_stuck_not_absent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a-directory.so");
    std::fs::create_dir(&path).unwrap();
    let target = CodecTarget {
        os: "test",
        arch: "test",
        sha256: "irrelevant-because-the-file-cannot-be-read",
        file_name: "a-directory.so",
    };

    assert!(matches!(
        verify_installed(&path, &target),
        Verified::Stuck(_)
    ));
}

/// An absolute `file_name` replaces the codec directory in the `join` of `installed_path`: the
/// test hashes its own temp file and never reads the real data directory.
#[tokio::test]
async fn the_supervisors_verify_hashes_the_file_at_the_installed_path() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("installed-bytes.so");
    let bytes = b"the bytes the digest was taken over";
    std::fs::write(&path, bytes).unwrap();
    let target: &'static CodecTarget = Box::leak(Box::new(CodecTarget {
        os: "test",
        arch: "test",
        sha256: Box::leak(sha256_bytes(bytes).into_boxed_str()),
        file_name: Box::leak(path.to_str().unwrap().to_owned().into_boxed_str()),
    }));

    let on_disk = OnDisk {
        target: Some(target),
    };

    assert!(matches!(on_disk.verify().await, Some(Verified::Installed(p)) if p == path));
    assert!(OnDisk { target: None }.verify().await.is_none());
}

/// Binds a listener that answers its first request with `response` verbatim, then drops the
/// socket.
async fn answering_port(response: &'static [u8]) -> u16 {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut request = [0u8; 1024];
        let _ = sock.read(&mut request).await;
        let _ = sock.write_all(response).await;
        let _ = sock.flush().await;
    });
    port
}

/// The redirect policy refuses through the same `send()` error as an unreachable host.
#[tokio::test]
async fn a_redirect_off_the_codec_host_is_a_refusal() {
    let port = answering_port(
        b"HTTP/1.1 302 Found\r\nLocation: https://example.invalid/x.bz2\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    )
    .await;

    let e = download_archive(&format!("http://127.0.0.1:{port}/x.bz2"))
        .await
        .unwrap_err();

    assert!(matches!(e, FetchError::Redirected(_)), "{e}");
    assert_eq!(e.class(), Some(FailureClass::Refused));
}

fn secs(n: u64) -> std::time::Duration {
    std::time::Duration::from_secs(n)
}

fn failed_phase(class: FailureClass) -> Phase {
    Phase::Unavailable(Unavailable::Failed(class))
}

#[test]
fn a_demand_waits_out_the_floor_of_its_class() {
    use Why::Demand;
    let network = failed_phase(FailureClass::Network);
    let disk = failed_phase(FailureClass::Disk);

    // Network: the fixed floor, whatever backoff the timer is on.
    assert_eq!(
        plan(
            &network,
            true,
            true,
            Demand,
            false,
            Some(secs(299)),
            secs(1800)
        ),
        Plan::Hold
    );
    assert_eq!(
        plan(
            &network,
            true,
            true,
            Demand,
            false,
            Some(secs(300)),
            secs(1800)
        ),
        Plan::Verify
    );
    // The others ride the backoff the last attempt earned.
    assert_eq!(
        plan(&disk, true, true, Demand, false, Some(secs(30)), secs(120)),
        Plan::Hold
    );
    assert_eq!(
        plan(&disk, true, true, Demand, false, Some(secs(120)), secs(120)),
        Plan::Verify
    );
    // Unloadable: no demand ever asks again.
    let unloadable = failed_phase(FailureClass::Unloadable);
    assert_eq!(
        plan(&unloadable, true, true, Demand, false, None, FIRST_RETRY),
        Plan::Hold
    );
}

#[test]
fn only_a_due_timer_on_a_network_failure_verifies() {
    use Why::Timer;
    let network = failed_phase(FailureClass::Network);

    assert_eq!(
        plan(&network, true, true, Timer, true, None, FIRST_RETRY),
        Plan::Verify
    );
    // A stale wake-up must not beat a backoff a later attempt extended.
    assert_eq!(
        plan(&network, true, true, Timer, false, None, FIRST_RETRY),
        Plan::Hold
    );
    let refused = failed_phase(FailureClass::Refused);
    assert_eq!(
        plan(&refused, true, true, Timer, true, None, FIRST_RETRY),
        Plan::Hold
    );
}

/// Off and on is the manual retry of a failure no timer or demand would take again this soon.
#[test]
fn the_switch_retries_every_failure_class() {
    for class in CLASSES {
        assert_eq!(
            plan(
                &failed_phase(class),
                true,
                true,
                Why::Switch,
                false,
                Some(secs(1)),
                MAX_RETRY
            ),
            Plan::Verify,
            "{class:?}"
        );
    }
}

#[test]
fn a_ready_codec_is_verified_again_only_on_a_recheck() {
    let ready = Phase::Ready(PathBuf::from("codec"));
    for why in [Why::Boot, Why::Timer, Why::Demand, Why::Switch] {
        assert_eq!(
            plan(&ready, true, true, why, true, None, FIRST_RETRY),
            Plan::Hold,
            "{why:?}"
        );
    }
    assert_eq!(
        plan(&ready, true, true, Why::Recheck, false, None, FIRST_RETRY),
        Plan::Verify
    );
}

#[test]
fn the_switch_and_the_platform_come_before_any_phase() {
    let ready = Phase::Ready(PathBuf::from("codec"));
    assert_eq!(
        plan(&ready, false, true, Why::Demand, false, None, FIRST_RETRY),
        Plan::Off
    );
    assert_eq!(
        plan(
            &Phase::Starting,
            true,
            false,
            Why::Boot,
            false,
            None,
            FIRST_RETRY
        ),
        Plan::Unsupported
    );
    assert_eq!(
        plan(
            &Phase::Starting,
            true,
            true,
            Why::Boot,
            false,
            None,
            FIRST_RETRY
        ),
        Plan::Verify
    );
}

/// What a scripted fetch fails with: attempt number (from 1) and the virtual time since the start.
type Failing = fn(usize, std::time::Duration) -> Option<FetchError>;

struct Scripted {
    t0: tokio::time::Instant,
    latency: std::time::Duration,
    failing: Failing,
    installed: std::sync::atomic::AtomicBool,
    /// The installed file can be neither read nor replaced.
    stuck: std::sync::atomic::AtomicBool,
    /// The first verify finds a file that failed its digest and removes it.
    removed_once: std::sync::atomic::AtomicBool,
    loads: std::sync::atomic::AtomicBool,
    fetches: std::sync::atomic::AtomicUsize,
    verifies: std::sync::atomic::AtomicUsize,
    probes: std::sync::atomic::AtomicUsize,
    loud: std::sync::atomic::AtomicUsize,
}

/// The supervisor gets one clone, the test keeps the other to read the counts.
#[derive(Clone)]
struct FakeIo(std::sync::Arc<Scripted>);

impl FakeIo {
    fn new(latency: std::time::Duration, failing: Failing) -> Self {
        use std::sync::atomic::{AtomicBool, AtomicUsize};
        Self(std::sync::Arc::new(Scripted {
            t0: tokio::time::Instant::now(),
            latency,
            failing,
            installed: AtomicBool::new(false),
            stuck: AtomicBool::new(false),
            removed_once: AtomicBool::new(false),
            loads: AtomicBool::new(true),
            fetches: AtomicUsize::new(0),
            verifies: AtomicUsize::new(0),
            probes: AtomicUsize::new(0),
            loud: AtomicUsize::new(0),
        }))
    }

    fn count(&self, counter: impl Fn(&Scripted) -> &std::sync::atomic::AtomicUsize) -> usize {
        counter(&self.0).load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl Io for FakeIo {
    fn target_known(&self) -> bool {
        true
    }

    async fn verify(&self) -> Option<Verified> {
        use std::sync::atomic::Ordering::SeqCst;
        self.0.verifies.fetch_add(1, SeqCst);
        if self.0.stuck.load(SeqCst) {
            let e = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
            return Some(Verified::Stuck(e));
        }
        if self.0.removed_once.swap(false, SeqCst) {
            return Some(Verified::Removed);
        }
        Some(if self.0.installed.load(SeqCst) {
            Verified::Installed(PathBuf::from("codec"))
        } else {
            Verified::Absent
        })
    }

    async fn fetch(&self) -> Result<PathBuf, FetchError> {
        use std::sync::atomic::Ordering::SeqCst;
        let attempt = self.0.fetches.fetch_add(1, SeqCst) + 1;
        tokio::time::sleep(self.0.latency).await;
        match (self.0.failing)(attempt, self.0.t0.elapsed()) {
            Some(e) => Err(e),
            None => {
                self.0.installed.store(true, SeqCst);
                Ok(PathBuf::from("codec"))
            }
        }
    }

    async fn probe(&self, _path: PathBuf) -> Option<anyhow::Result<()>> {
        use std::sync::atomic::Ordering::SeqCst;
        self.0.probes.fetch_add(1, SeqCst);
        Some(if self.0.loads.load(SeqCst) {
            Ok(())
        } else {
            Err(anyhow::anyhow!("the loader refused it"))
        })
    }

    fn report(&self, gate: Gate, _err: anyhow::Error) {
        if gate == Gate::Loud {
            self.0
                .loud
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }
}

fn never_fails(_: usize, _: std::time::Duration) -> Option<FetchError> {
    None
}

fn not_found(_: usize, _: std::time::Duration) -> Option<FetchError> {
    Some(FetchError::Status(reqwest::StatusCode::NOT_FOUND))
}

fn unavailable(_: usize, _: std::time::Duration) -> Option<FetchError> {
    Some(FetchError::Status(reqwest::StatusCode::SERVICE_UNAVAILABLE))
}

fn supervising(io: &FakeIo) -> std::sync::Arc<Supervisor> {
    let rt = tokio::runtime::Handle::current();
    std::sync::Arc::new(Supervisor::start(&rt, io.clone(), true))
}

fn phase(supervisor: &Supervisor) -> Phase {
    supervisor.state.borrow().phase.clone()
}

const DAY: u64 = 24 * 3600;

/// `horizon` seconds of virtual time. Active: the filter's nudge every 5 s, and a cover job (a
/// real ask) once a minute. At rest: nothing asks.
async fn live(supervisor: &std::sync::Arc<Supervisor>, active: bool, horizon: u64) {
    let mut elapsed = 0;
    let mut ticks = 0u64;
    while elapsed < horizon {
        tokio::time::sleep(secs(5)).await;
        elapsed += 5;
        ticks += 1;
        if active {
            supervisor.demand();
            if ticks.is_multiple_of(12) {
                let asking = supervisor.clone();
                tokio::spawn(async move {
                    let _ = asking.ask().await;
                });
            }
        }
    }
}

/// A refusal asks once at rest and rides the backoff under demand (the cover jobs used to download
/// again at every ask). A blocked host under demand is asked once per floor, after the early
/// backoffs the timer's doubling adds. Each row logs once.
#[tokio::test(start_paused = true)]
async fn a_failing_fetch_is_asked_again_by_its_class_and_logged_once() {
    let rows: [(Failing, bool, Phase, usize); 3] = [
        (not_found, false, failed_phase(FailureClass::Refused), 1),
        (not_found, true, failed_phase(FailureClass::Refused), 53),
        (unavailable, true, failed_phase(FailureClass::Network), 291),
    ];
    for (failing, active, expected, fetches) in rows {
        let io = FakeIo::new(secs(1), failing);
        let supervisor = supervising(&io);

        live(&supervisor, active, DAY).await;

        assert_eq!(phase(&supervisor), expected, "active: {active}");
        assert_eq!(io.count(|s| &s.fetches), fetches, "active: {active}");
        assert_eq!(io.count(|s| &s.loud), 1, "active: {active}");
    }
}

#[tokio::test(start_paused = true)]
async fn the_timer_recovers_a_network_that_comes_back() {
    fn offline_for_20_minutes(_: usize, since: std::time::Duration) -> Option<FetchError> {
        (since < secs(1200)).then(|| FetchError::Status(reqwest::StatusCode::SERVICE_UNAVAILABLE))
    }
    let io = FakeIo::new(secs(1), offline_for_20_minutes);
    let supervisor = supervising(&io);

    live(&supervisor, false, 4 * 3600).await;

    assert!(supervisor.is_ready());
    assert_eq!(io.count(|s| &s.fetches), 7);
    assert_eq!(io.count(|s| &s.loud), 1);
}

/// A rename held longer than `write_atomically` waits, twice, then free: a demand takes it again.
#[tokio::test(start_paused = true)]
async fn a_transient_lock_is_taken_again_on_demand() {
    fn locked_twice(attempt: usize, _: std::time::Duration) -> Option<FetchError> {
        (attempt <= 2)
            .then(|| FetchError::Save(std::io::Error::from(std::io::ErrorKind::PermissionDenied)))
    }
    let io = FakeIo::new(secs(1), locked_twice);
    let supervisor = supervising(&io);

    live(&supervisor, true, 3600).await;

    assert!(supervisor.is_ready());
    assert_eq!(io.count(|s| &s.fetches), 3);
    assert_eq!(io.count(|s| &s.loud), 1);
}

/// The old `status()` wrote a `verr!` at every call: 1440 lines a day.
#[tokio::test(start_paused = true)]
async fn a_stuck_installed_file_is_never_downloaded_over_and_logs_once() {
    let io = FakeIo::new(secs(1), never_fails);
    io.0.stuck.store(true, std::sync::atomic::Ordering::SeqCst);
    let supervisor = supervising(&io);

    live(&supervisor, true, DAY).await;

    assert_eq!(phase(&supervisor), failed_phase(FailureClass::Disk));
    assert_eq!(io.count(|s| &s.fetches), 0);
    assert_eq!(io.count(|s| &s.loud), 1);
}

#[tokio::test(start_paused = true)]
async fn fifty_cold_asks_share_one_download() {
    let io = FakeIo::new(secs(5), never_fails);
    let supervisor = supervising(&io);

    let asks: Vec<_> = (0..50)
        .map(|_| {
            let asking = supervisor.clone();
            tokio::spawn(async move { asking.ask().await })
        })
        .collect();
    for ask in asks {
        assert_eq!(ask.await.unwrap(), Ok(PathBuf::from("codec")));
    }

    assert_eq!(io.count(|s| &s.fetches), 1);
    assert_eq!(io.count(|s| &s.probes), 1);
}

/// The environment does not change while the process runs: probed once, never fetched again.
#[tokio::test(start_paused = true)]
async fn an_unloadable_codec_is_probed_once() {
    let io = FakeIo::new(secs(1), never_fails);
    io.0.loads.store(false, std::sync::atomic::Ordering::SeqCst);
    let supervisor = supervising(&io);

    live(&supervisor, true, 3600).await;

    assert_eq!(phase(&supervisor), failed_phase(FailureClass::Unloadable));
    assert_eq!(io.count(|s| &s.fetches), 1);
    assert_eq!(io.count(|s| &s.probes), 1);
    assert_eq!(io.count(|s| &s.verifies), 1);
}

#[tokio::test(start_paused = true)]
async fn a_file_removed_at_boot_is_downloaded_at_once() {
    let io = FakeIo::new(secs(1), never_fails);
    io.0.removed_once
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let supervisor = supervising(&io);

    live(&supervisor, false, 60).await;

    assert!(supervisor.is_ready());
    assert_eq!(io.count(|s| &s.fetches), 1);
}

#[tokio::test(start_paused = true)]
async fn a_switch_off_at_boot_fetches_nothing() {
    let io = FakeIo::new(secs(1), never_fails);
    let rt = tokio::runtime::Handle::current();
    let supervisor = Supervisor::start(&rt, io.clone(), false);

    assert_eq!(
        supervisor.ask().await,
        Err(NoDecoder::Unavailable(Unavailable::Off))
    );
    assert_eq!(io.count(|s| &s.fetches), 0);
    assert_eq!(io.count(|s| &s.verifies), 0);
}

#[tokio::test(start_paused = true)]
async fn the_switch_turns_the_codec_off_and_on_live() {
    let io = FakeIo::new(secs(3), never_fails);
    let supervisor = supervising(&io);
    live(&supervisor, false, 10).await;
    assert!(supervisor.is_ready());

    supervisor.set_enabled(false);
    live(&supervisor, false, 5).await;
    assert_eq!(
        supervisor.ask().await,
        Err(NoDecoder::Unavailable(Unavailable::Off))
    );
    assert!(!supervisor.is_ready());
    assert!(
        !supervisor.state.borrow().enabled,
        "the echo the page reads"
    );

    supervisor.set_enabled(true);
    live(&supervisor, false, 10).await;
    assert!(supervisor.is_ready());
    assert!(supervisor.state.borrow().enabled);
    // Off keeps the file: on again only verifies it.
    assert_eq!(io.count(|s| &s.fetches), 1);
}

/// A failure is the phase where every switch message plans a retry: the burst is one retry, not
/// one download per click, whatever `NETWORK_DEMAND_FLOOR` says of the demands. A burst that ends
/// on off makes no retry at all.
#[tokio::test(start_paused = true)]
async fn a_burst_of_toggles_on_a_failure_retries_once_or_not_at_all() {
    for (toggles, expected, fetches) in [
        ([false, true], failed_phase(FailureClass::Network), 2),
        ([true, false], Phase::Unavailable(Unavailable::Off), 1),
    ] {
        let io = FakeIo::new(secs(1), unavailable);
        let supervisor = supervising(&io);
        live(&supervisor, false, 5).await;
        assert_eq!(io.count(|s| &s.fetches), 1, "{toggles:?}");

        for _ in 0..5 {
            for on in toggles {
                supervisor.set_enabled(on);
            }
        }
        live(&supervisor, false, 20).await;

        assert_eq!(phase(&supervisor), expected, "{toggles:?}");
        assert_eq!(io.count(|s| &s.fetches), fetches, "{toggles:?}");
    }
}

/// Folding a burst must not carry a switch past an ask queued behind it: the ask is answered
/// after the retry the first click made, and the click behind it retries nothing more.
#[tokio::test(start_paused = true)]
async fn an_ask_between_two_switches_is_answered_after_the_first_retry() {
    fn refused_once(attempt: usize, _: std::time::Duration) -> Option<FetchError> {
        (attempt == 1).then_some(FetchError::Status(reqwest::StatusCode::NOT_FOUND))
    }
    let io = FakeIo::new(secs(1), refused_once);
    let supervisor = supervising(&io);
    live(&supervisor, false, 5).await;
    assert_eq!(phase(&supervisor), failed_phase(FailureClass::Refused));

    supervisor.set_enabled(true);
    let mut ask = Box::pin(supervisor.ask());
    std::future::poll_fn(|cx| {
        assert!(ask.as_mut().poll(cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    supervisor.set_enabled(true);

    assert_eq!(ask.await, Ok(PathBuf::from("codec")));
    live(&supervisor, false, 5).await;
    assert!(supervisor.is_ready());
    assert_eq!(io.count(|s| &s.fetches), 2);
}

/// The task is busy with the download: `Off` follows it, and the file it brought stays.
#[tokio::test(start_paused = true)]
async fn a_switch_off_during_a_download_lands_after_it() {
    let io = FakeIo::new(secs(40), never_fails);
    let supervisor = supervising(&io);
    live(&supervisor, false, 5).await;
    assert_eq!(phase(&supervisor), Phase::Downloading);

    supervisor.set_enabled(false);
    live(&supervisor, false, 60).await;

    assert_eq!(phase(&supervisor), Phase::Unavailable(Unavailable::Off));
    assert!(io.0.installed.load(std::sync::atomic::Ordering::SeqCst));
    assert_eq!(io.count(|s| &s.fetches), 1);
}

/// The one way back from `Unloadable` short of a relaunch: a system library may have changed.
#[tokio::test(start_paused = true)]
async fn the_switch_probes_an_unloadable_codec_again() {
    let io = FakeIo::new(secs(1), never_fails);
    io.0.loads.store(false, std::sync::atomic::Ordering::SeqCst);
    let supervisor = supervising(&io);
    live(&supervisor, false, 5).await;
    assert_eq!(phase(&supervisor), failed_phase(FailureClass::Unloadable));

    io.0.loads.store(true, std::sync::atomic::Ordering::SeqCst);
    supervisor.set_enabled(false);
    supervisor.set_enabled(true);
    live(&supervisor, false, 5).await;

    assert!(supervisor.is_ready());
    assert_eq!(io.count(|s| &s.probes), 2);
}

/// Panics where a test arms it, through the real `off_runtime` where production goes there:
/// `FakeIo` never reaches the blocking pool.
#[derive(Clone, Default)]
struct Crashing(std::sync::Arc<Panics>);

#[derive(Default)]
struct Panics {
    installed: std::sync::atomic::AtomicBool,
    fetch_once: std::sync::atomic::AtomicBool,
    verify_after_removal_once: std::sync::atomic::AtomicBool,
    verify_always: std::sync::atomic::AtomicBool,
    report: std::sync::atomic::AtomicBool,
    refused: std::sync::atomic::AtomicBool,
    verifies: std::sync::atomic::AtomicUsize,
    fetches: std::sync::atomic::AtomicUsize,
}

impl Crashing {
    fn arm(&self, flag: impl Fn(&Panics) -> &std::sync::atomic::AtomicBool) {
        flag(&self.0).store(true, std::sync::atomic::Ordering::SeqCst);
    }

    fn count(&self, counter: impl Fn(&Panics) -> &std::sync::atomic::AtomicUsize) -> usize {
        counter(&self.0).load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl Io for Crashing {
    fn target_known(&self) -> bool {
        true
    }

    async fn verify(&self) -> Option<Verified> {
        use std::sync::atomic::Ordering::SeqCst;
        let panics = self.0.clone();
        panics.verifies.fetch_add(1, SeqCst);
        off_runtime(move || {
            if panics.verify_always.load(SeqCst) {
                panic!("verify panics every time");
            }
            if !panics.installed.load(SeqCst) {
                return Verified::Absent;
            }
            if panics.verify_after_removal_once.swap(false, SeqCst) {
                panics.installed.store(false, SeqCst);
                panic!("verify panics after removing the file");
            }
            Verified::Installed(PathBuf::from("codec"))
        })
        .await
    }

    async fn fetch(&self) -> Result<PathBuf, FetchError> {
        use std::sync::atomic::Ordering::SeqCst;
        let panics = self.0.clone();
        panics.fetches.fetch_add(1, SeqCst);
        if panics.refused.load(SeqCst) {
            return Err(FetchError::Status(reqwest::StatusCode::NOT_FOUND));
        }
        off_runtime(move || {
            if panics.fetch_once.swap(false, SeqCst) {
                panic!("fetch panics once");
            }
            panics.installed.store(true, SeqCst);
            Ok(PathBuf::from("codec"))
        })
        .await
        .unwrap_or(Err(FetchError::Shutdown))
    }

    async fn probe(&self, _path: PathBuf) -> Option<anyhow::Result<()>> {
        off_runtime(|| Ok(())).await
    }

    fn report(&self, _gate: Gate, _err: anyhow::Error) {
        if self.0.report.load(std::sync::atomic::Ordering::SeqCst) {
            panic!("the logger panics");
        }
    }
}

fn supervising_crashes(io: &Crashing) -> std::sync::Arc<Supervisor> {
    let rt = tokio::runtime::Handle::current();
    std::sync::Arc::new(Supervisor::start(&rt, io.clone(), true))
}

/// What a cover job gets: an answer, which a dead task would not give past `Stopped`.
async fn asked(supervisor: &Supervisor) -> Result<PathBuf, NoDecoder> {
    tokio::time::timeout(secs(30), supervisor.ask())
        .await
        .expect("a live supervisor answers")
}

/// A dead task used to leave `Downloading` on the page and `Stopped` in every cover job until
/// a relaunch.
#[tokio::test(start_paused = true)]
async fn a_panic_on_the_blocking_pool_fails_crashed_and_a_demand_heals_it() {
    let io = Crashing::default();
    io.arm(|p| &p.fetch_once);
    let supervisor = supervising_crashes(&io);
    live(&supervisor, false, 5).await;

    assert_eq!(phase(&supervisor), failed_phase(FailureClass::Crashed));
    assert_eq!(asked(&supervisor).await, Err(failed(FailureClass::Crashed)));

    // Past the backoff's first 30 s.
    live(&supervisor, false, 30).await;
    supervisor.demand();
    live(&supervisor, false, 5).await;

    assert!(supervisor.is_ready());
    assert_eq!(io.count(|p| &p.fetches), 2);
}

/// The variant a dead task made worst: `Ready` kept vouching for a file the panicking recheck had
/// already removed.
#[tokio::test(start_paused = true)]
async fn a_recheck_that_panics_after_removing_the_file_stops_vouching_for_it() {
    let io = Crashing::default();
    io.arm(|p| &p.installed);
    let supervisor = supervising_crashes(&io);
    live(&supervisor, false, 5).await;
    assert!(supervisor.is_ready());

    io.arm(|p| &p.verify_after_removal_once);
    supervisor.recheck();
    live(&supervisor, false, 5).await;

    assert!(!supervisor.is_ready());
    assert_eq!(phase(&supervisor), failed_phase(FailureClass::Crashed));

    live(&supervisor, false, 30).await;
    supervisor.demand();
    live(&supervisor, false, 5).await;

    assert!(supervisor.is_ready());
    assert_eq!(io.count(|p| &p.fetches), 1);
}

/// The logger was the panic that killed the task: the failure is published before it is logged.
#[tokio::test(start_paused = true)]
async fn a_panicking_logger_still_publishes_the_failure() {
    let io = Crashing::default();
    io.arm(|p| &p.refused);
    io.arm(|p| &p.report);
    let supervisor = supervising_crashes(&io);
    live(&supervisor, false, 5).await;

    assert_eq!(phase(&supervisor), failed_phase(FailureClass::Crashed));
    assert_eq!(asked(&supervisor).await, Err(failed(FailureClass::Crashed)));
}

/// The same 53 attempts a day as a refusal: a panic that recurs every time rides the backoff.
#[tokio::test(start_paused = true)]
async fn a_step_that_always_panics_rides_the_backoff() {
    let io = Crashing::default();
    io.arm(|p| &p.verify_always);
    let supervisor = supervising_crashes(&io);

    live(&supervisor, true, DAY).await;

    assert_eq!(phase(&supervisor), failed_phase(FailureClass::Crashed));
    assert_eq!(io.count(|p| &p.verifies), 53);
}

fn json_of(phase: Phase) -> serde_json::Value {
    let snapshot = Snapshot {
        seq: 7,
        enabled: true,
        phase,
    };
    serde_json::to_value(view(&snapshot)).unwrap()
}

/// The exact shape the settings row reads: a path never leaves, a failure names its recovery.
#[test]
fn the_view_the_page_reads_has_one_shape_per_phase() {
    use serde_json::json;

    assert_eq!(
        json_of(Phase::Starting),
        json!({"seq": 7, "enabled": true, "phase": {"kind": "starting"}})
    );
    assert_eq!(
        json_of(Phase::Downloading)["phase"],
        json!({"kind": "downloading"})
    );
    assert_eq!(
        json_of(Phase::Ready(PathBuf::from(
            "/nonexistent/codec-path-marker"
        )))["phase"],
        json!({"kind": "ready"})
    );
    assert_eq!(
        json_of(Phase::Unavailable(Unavailable::Off))["phase"],
        json!({"kind": "off"})
    );
    assert_eq!(
        json_of(Phase::Unavailable(Unavailable::Unsupported))["phase"],
        json!({"kind": "unsupported"})
    );
    for (class, recovery) in [
        (FailureClass::Network, "timer"),
        (FailureClass::Refused, "on_demand"),
        (FailureClass::Corrupt, "on_demand"),
        (FailureClass::Disk, "on_demand"),
        (FailureClass::Unloadable, "never"),
        (FailureClass::Crashed, "on_demand"),
    ] {
        assert_eq!(
            json_of(failed_phase(class))["phase"],
            json!({"kind": "failed", "reason": class.phrase(), "recovery": recovery}),
            "{class:?}"
        );
    }
}

#[test]
fn only_a_change_moves_the_sequence() {
    let (tx, rx) = tokio::sync::watch::channel(Snapshot {
        seq: 0,
        enabled: true,
        phase: Phase::Starting,
    });

    publish(&tx, Phase::Downloading, true);
    assert_eq!(rx.borrow().seq, 1);
    publish(&tx, Phase::Downloading, true);
    assert_eq!(rx.borrow().seq, 1, "the same snapshot again is no news");
    publish(&tx, failed_phase(FailureClass::Network), true);
    assert_eq!(rx.borrow().seq, 2);
    // The switch alone is news: the page reads its position from the echo.
    publish(&tx, failed_phase(FailureClass::Network), false);
    assert_eq!(rx.borrow().seq, 3);
    assert!(!rx.borrow().enabled);
}
