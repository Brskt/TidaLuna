use std::sync::{Arc, LazyLock};
use std::time::Duration;

/// Chromium version CEF wraps, `MAJOR.MINOR.BUILD.PATCH`. This is not the `cef` crate
/// version: the crate tracks CEF releases, which wrap their own Chromium. One owner keeps
/// the user agent and the startup log from disagreeing about which browser this is.
pub(crate) fn chromium_version() -> String {
    format!(
        "{}.{}.{}.{}",
        cef::sys::CHROME_VERSION_MAJOR,
        cef::sys::CHROME_VERSION_MINOR,
        cef::sys::CHROME_VERSION_BUILD,
        cef::sys::CHROME_VERSION_PATCH,
    )
}

fn build_user_agent(os_token: &str) -> String {
    format!(
        "Mozilla/5.0 ({os}) AppleWebKit/537.36 (KHTML, like Gecko) TidaLunar/{ver} Chrome/{chrome} Safari/537.36",
        os = os_token,
        ver = env!("CARGO_PKG_VERSION"),
        chrome = chromium_version(),
    )
}

pub(crate) static USER_AGENT: LazyLock<String> = LazyLock::new(|| {
    let os = if cfg!(target_os = "linux") {
        "X11; Linux x86_64"
    } else if cfg!(target_os = "macos") {
        "Macintosh; Intel Mac OS X 10_15_7"
    } else {
        "Windows NT 10.0; WOW64"
    };
    build_user_agent(os)
});

/// Shared client tuning (UA, timeouts, pooling, HTTP/2, TLS) minus the cookie
/// configuration, which the global and per-plugin clients set differently.
fn base_client_builder() -> reqwest::ClientBuilder {
    // Keep streaming requests unconstrained (no global request timeout),
    // but tune connection setup and pooling for lower latency variance.
    reqwest::Client::builder()
        .user_agent(USER_AGENT.as_str())
        .connect_timeout(Duration::from_secs(8))
        .pool_idle_timeout(Duration::from_secs(90))
        .pool_max_idle_per_host(8)
        .tcp_nodelay(true)
        // HTTP/2: adaptive flow-control window for better throughput
        .http2_adaptive_window(true)
        // HTTP/2: keep-alive pings to detect dead connections
        .http2_keep_alive_interval(Duration::from_secs(10))
        .http2_keep_alive_timeout(Duration::from_secs(5))
        .http2_keep_alive_while_idle(true)
        // TLS: accept 1.2 and 1.3
        .min_tls_version(reqwest::tls::Version::TLS_1_2)
}

fn build_http_client() -> reqwest::Client {
    base_client_builder()
        .cookie_store(true)
        .build()
        .expect("failed to build HTTP client")
}

/// The clients that stream an encrypted media body, which is the only traffic here with an
/// unbounded await on a body that a silent server can hold open forever.
///
/// A whole-request timeout is the wrong shape, one body being a whole track. `read_timeout`'s
/// clock is created inside `poll_frame` and torn down on every delivered frame, so the stretch
/// the governor spends not polling costs no budget, and its separate one-shot deadline ends a
/// server that accepts the connection and then answers nothing. 15s clears the 8s connect budget
/// with room for a cold CDN edge's TTFB. Deliberately not on `base_client_builder`: the plugin
/// egress clients layer on that one, and a plugin's destination may answer at any cadence.
fn build_media_client() -> reqwest::Client {
    base_client_builder()
        .cookie_store(true)
        .read_timeout(Duration::from_secs(15))
        .build()
        .expect("failed to build media HTTP client")
}

/// Build a native-plugin egress client with a caller-owned cookie jar (for
/// per-plugin isolation) and a redirect policy (follow vs manual/error).
pub(crate) fn build_native_client(
    jar: Arc<reqwest::cookie::Jar>,
    redirect: reqwest::redirect::Policy,
) -> reqwest::Client {
    base_client_builder()
        .cookie_provider(jar)
        .redirect(redirect)
        .build()
        .expect("failed to build native HTTP client")
}

/// URLs one request may visit, the first included (`Attempt::previous` counts it). A redirect
/// past the cap comes back as the 3xx response, for the caller's status check to reject.
const MAX_REDIRECT_CHAIN: usize = 10;

/// A client that re-checks `allowed` at every redirect hop and refuses with
/// [`crate::util::Refused`]`(refusal)` the moment a hop leaves it. Checking the submitted URL alone
/// would be bypassed by an open redirect on an allowed host. No cookie store.
pub(crate) fn build_host_checked_client(
    allowed: fn(&url::Url) -> bool,
    refusal: &'static str,
) -> reqwest::Client {
    base_client_builder()
        .redirect(reqwest::redirect::Policy::custom(move |attempt| {
            if !allowed(attempt.url()) {
                return attempt.error(crate::util::Refused(refusal));
            }
            if attempt.previous().len() >= MAX_REDIRECT_CHAIN {
                return attempt.stop();
            }
            attempt.follow()
        }))
        .build()
        .expect("failed to build redirect-checked HTTP client")
}

pub static HTTP_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(build_http_client);

/// Dedicated HTTP client for playback (initial GET + Range restarts).
/// Separate from HTTP_CLIENT; playback gets its own TCP connection,
/// avoiding HTTP/2 bandwidth contention with preload downloads.
pub static HTTP_CLIENT_PLAYBACK: LazyLock<reqwest::Client> = LazyLock::new(build_media_client);

/// Preload's own client, kept off `HTTP_CLIENT` for the read timeout rather than for contention:
/// its download loop has the same governor-then-chunk shape as playback's and the same await a
/// silent server can hold open, while `HTTP_CLIENT` also serves `plugin.fetch`.
pub static HTTP_CLIENT_PRELOAD: LazyLock<reqwest::Client> = LazyLock::new(build_media_client);
