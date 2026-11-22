//! Tests for `src/state/net.rs`, attached to it by `#[path]`.

use super::*;
use crate::state::HANG_GUARD;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Only a hang reaches it in real time. The bound under test elapses on a paused clock instead.
const SILENCE: Duration = HANG_GUARD;

/// What test 2's fixture sends before it goes silent.
const SENT: usize = 1024;

/// Answers one request with a head, then with `body` until the socket fails, then stays open
/// without a byte more. Returns the url to fetch.
async fn media_server(body: Option<usize>) -> String {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut request = [0u8; 1024];
        let _ = sock.read(&mut request).await;
        let _ = sock
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1073741824\r\n\r\n")
            .await;
        match body {
            // As a CDN does: only TCP flow control holds it back while the client pauses.
            None => while sock.write_all(&[0u8; 16 * 1024]).await.is_ok() {},
            Some(len) => {
                let _ = sock.write_all(&vec![0u8; len]).await;
            }
        }
        let _ = sock.flush().await;
        std::future::pending::<()>().await;
    });
    format!("http://127.0.0.1:{port}/track.flac")
}

/// Answers each connection with `answer(n)`, `n` counting requests from 0, and records each
/// request head.
async fn scripted_server(
    answer: fn(usize) -> String,
) -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let record = seen.clone();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let mut head = Vec::new();
            let mut chunk = [0u8; 4096];
            while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                match sock.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => head.extend_from_slice(&chunk[..n]),
                }
            }
            let n = {
                let mut seen = record.lock().unwrap();
                seen.push(String::from_utf8_lossy(&head).to_ascii_lowercase());
                seen.len() - 1
            };
            let _ = sock.write_all(answer(n).as_bytes()).await;
            let _ = sock.shutdown().await;
        }
    });
    (format!("http://127.0.0.1:{port}"), seen)
}

fn redirect_to(status: &str, location: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    )
}

fn in_chain<E: std::error::Error + 'static>(err: &reqwest::Error) -> bool {
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(e) = source {
        if e.is::<E>() {
            return true;
        }
        source = e.source();
    }
    false
}

fn permitted(url: &str) -> PermittedUrl {
    PermittedUrl::new(url::Url::parse(url).unwrap()).unwrap()
}

// An open redirect on a host a plugin may reach must not carry its request, body and all, to a
// sign-in host. The hop is refused before it is sent: the record holds the first request alone.
#[tokio::test]
async fn a_plugin_request_never_follows_a_redirect_to_a_sign_in_host() {
    let (base, seen) =
        scripted_server(|_| redirect_to("302 Found", "https://auth.tidal.com/v1/x")).await;
    let err = PluginClient::build()
        .request(reqwest::Method::POST, permitted(&format!("{base}/start")))
        .body("refresh_token=secret")
        .send()
        .await
        .expect_err("the hop is refused");
    assert!(in_chain::<crate::util::Refused>(&err), "{err:?}");
    assert_eq!(seen.lock().unwrap().len(), 1);
}

// The cap is no refusal: a chain that never ends is told apart from a forbidden host, and every
// hop up to the cap is followed.
#[tokio::test]
async fn a_redirect_chain_past_the_cap_fails_as_too_many_redirects() {
    let (base, seen) =
        scripted_server(|n| redirect_to("302 Found", &format!("/hop{}", n + 1))).await;
    let err = PluginClient::build()
        .request(reqwest::Method::GET, permitted(&format!("{base}/hop0")))
        .send()
        .await
        .expect_err("an endless chain must fail");
    assert!(in_chain::<TooManyRedirects>(&err), "{err:?}");
    assert!(!in_chain::<crate::util::Refused>(&err), "{err:?}");
    assert_eq!(seen.lock().unwrap().len(), MAX_REDIRECT_HOPS + 1);
}

// Plugins keep their cookies across requests, as `plugin.fetch` does.
#[tokio::test]
async fn the_plugin_client_keeps_cookies_across_requests() {
    fn answer(_: usize) -> String {
        "HTTP/1.1 200 OK\r\nSet-Cookie: session=kept; Path=/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            .to_string()
    }
    let (base, seen) = scripted_server(answer).await;
    let plugin = PluginClient::build();
    for _ in 0..2 {
        plugin
            .request(reqwest::Method::GET, permitted(&format!("{base}/a")))
            .send()
            .await
            .unwrap();
    }
    assert!(seen.lock().unwrap()[1].contains("cookie: session=kept"));
}

// A media URL comes from TIDAL's answer or from a plugin. One naming a sign-in host, or a malformed
// TIDAL host, is refused before any connection: nothing listens at these names here.
#[tokio::test]
async fn a_media_url_on_a_sign_in_or_malformed_tidal_host_is_never_sent() {
    let client = MediaClient::with_silence(SILENCE);
    for (url, expected) in [
        (
            "https://auth.tidal.com/track.flac",
            EgressRefusal::SignInHost,
        ),
        (
            "https://lgf.audio.tidal.com./track.flac",
            EgressRefusal::MalformedTidalHost,
        ),
    ] {
        let outcome = client.get(url).send().await;
        assert!(
            matches!(outcome, Err(AudioFetchError::Refused(refusal)) if refusal == expected),
            "{url}: {:?}",
            outcome.err()
        );
    }
}

// The governor holds a download between two reads for as long as playback is paused. reqwest
// 0.13.5's `read_timeout` counted that wait, and every resume after 15 s reconnected for nothing.
#[tokio::test]
async fn a_pause_between_two_reads_costs_nothing() {
    let client = MediaClient::with_silence(SILENCE);
    let mut body = client
        .get(media_server(None).await)
        .send()
        .await
        .expect("the fixture answers")
        .body();
    body.next()
        .await
        .expect("a first frame")
        .expect("the first read succeeds");

    // The pause outlasts the bound in virtual time, while the read after it runs on the real clock.
    tokio::time::pause();
    tokio::time::advance(SILENCE * 2).await;
    tokio::time::resume();

    let after = body.next().await;
    assert!(
        matches!(after, Some(Ok(_))),
        "a read after the pause failed: {after:?}"
    );
}

// Without the bound, a server that keeps the connection and stops sending holds the download
// for good.
#[tokio::test]
async fn a_body_that_goes_silent_ends_at_the_bound() {
    let client = MediaClient::with_silence(SILENCE);
    let mut body = client
        .get(media_server(Some(SENT)).await)
        .send()
        .await
        .expect("the fixture answers")
        .body();
    let mut received = 0;
    while received < SENT {
        received += body
            .next()
            .await
            .expect("the body goes on")
            .expect("a read before the silence succeeds")
            .len();
    }

    // Every byte is in: what remains is the silence, and it elapses in virtual time.
    tokio::time::pause();
    let outcome = tokio::time::timeout(SILENCE * 4, async {
        loop {
            match body.next().await {
                Some(Ok(_)) => continue,
                other => break other,
            }
        }
    })
    .await
    .expect("a silent body must end");
    assert!(
        matches!(outcome, Some(Err(AudioFetchError::Stalled))),
        "a silent body ended otherwise: {outcome:?}"
    );
}
