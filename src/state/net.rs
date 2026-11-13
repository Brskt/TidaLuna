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

/// How long a media server may stay silent: no headers after a send, or no frame while a read
/// waits. 15 s clears the 8 s connect budget with room for a cold CDN edge's first byte.
const MEDIA_SILENCE: Duration = Duration::from_secs(15);

/// No `read_timeout` here: reqwest 0.13.5 counts the time between two reads against it, and the
/// download loops pause there on purpose for the governor. [`MediaBody::next`] bounds each read.
fn build_media_client(silence: Duration) -> MediaClient {
    let inner = base_client_builder()
        .cookie_store(true)
        .build()
        .expect("failed to build media HTTP client");
    MediaClient { inner, silence }
}

/// A client for encrypted media bodies, the one traffic here a silent server could hold open
/// forever. Every send and body read it hands out ends after `silence` with no data.
pub struct MediaClient {
    inner: reqwest::Client,
    silence: Duration,
}

impl MediaClient {
    pub fn get(&self, url: impl reqwest::IntoUrl) -> MediaRequest {
        MediaRequest {
            inner: self.inner.get(url),
            silence: self.silence,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_silence(silence: Duration) -> Self {
        build_media_client(silence)
    }
}

pub struct MediaRequest {
    inner: reqwest::RequestBuilder,
    silence: Duration,
}

impl MediaRequest {
    /// Asks for the body from `offset` to its end.
    pub fn starting_at(self, offset: u64) -> Self {
        Self {
            inner: self
                .inner
                .header(reqwest::header::RANGE, format!("bytes={offset}-")),
            ..self
        }
    }

    /// Ends a server that accepts the connection and never answers, its redirects included.
    pub async fn send(self) -> Result<MediaResponse, AudioFetchError> {
        match tokio::time::timeout(self.silence, self.inner.send()).await {
            Ok(Ok(inner)) => Ok(MediaResponse {
                inner,
                silence: self.silence,
            }),
            Ok(Err(e)) => Err(AudioFetchError::transport(e)),
            Err(_) => Err(AudioFetchError::NoAnswer),
        }
    }
}

/// Its body is reached through [`MediaResponse::body`] or [`MediaResponse::bytes`] alone: the
/// consuming reads of `reqwest::Response` cannot be called through `Deref`.
pub struct MediaResponse {
    inner: reqwest::Response,
    silence: Duration,
}

impl std::ops::Deref for MediaResponse {
    type Target = reqwest::Response;

    fn deref(&self) -> &reqwest::Response {
        &self.inner
    }
}

impl MediaResponse {
    pub fn body(self) -> MediaBody {
        MediaBody {
            inner: self.inner,
            silence: self.silence,
        }
    }

    /// Each read is bounded on its own, not the whole: a slow segment still sending finishes.
    pub async fn bytes(self) -> Result<Vec<u8>, AudioFetchError> {
        let mut body = self.body();
        let mut data = Vec::new();
        while let Some(chunk) = body.next().await {
            data.extend_from_slice(&chunk?);
        }
        Ok(data)
    }
}

/// A media body read one frame at a time. A whole-body timeout is the wrong shape, one body being
/// a whole track.
pub struct MediaBody {
    inner: reqwest::Response,
    silence: Duration,
}

impl MediaBody {
    /// The next frame, `None` at the end. The clock runs only while this waits: a pause between
    /// two reads costs nothing, and a read dropped mid-wait takes its clock with it.
    pub async fn next(&mut self) -> Option<Result<tokio_util::bytes::Bytes, AudioFetchError>> {
        match tokio::time::timeout(self.silence, self.inner.chunk()).await {
            Ok(Ok(chunk)) => chunk.map(Ok),
            Ok(Err(e)) => Some(Err(AudioFetchError::transport(e))),
            Err(_) => Some(Err(AudioFetchError::Stalled)),
        }
    }
}

/// Why a media send or read failed.
#[derive(Debug)]
pub enum AudioFetchError {
    /// No response headers within the silence bound of the send.
    NoAnswer,
    /// No frame within the silence bound while a read waited.
    Stalled,
    /// reqwest's own failure, its URL removed by [`crate::util::network_error_text`].
    Transport(String),
}

impl AudioFetchError {
    fn transport(e: reqwest::Error) -> Self {
        Self::Transport(crate::util::network_error_text(e))
    }
}

impl std::fmt::Display for AudioFetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoAnswer => f.write_str("the server sent no answer"),
            Self::Stalled => f.write_str("the server stopped sending"),
            Self::Transport(text) => f.write_str(text),
        }
    }
}

impl std::error::Error for AudioFetchError {}

/// The client for plugin sources. Its `read_timeout` ends a source that stops sending, a bound
/// `HTTP_CLIENT` cannot take since it also carries `plugin.fetch`. Its own jar keeps a source's
/// redirect cookies apart from those of `plugin.fetch`.
fn build_source_client() -> reqwest::Client {
    base_client_builder()
        .cookie_store(true)
        // From send() to the headers too, redirects included; 30 s is what the whole fetch had.
        .read_timeout(Duration::from_secs(30))
        .build()
        .expect("failed to build plugin source HTTP client")
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

pub(crate) static HTTP_CLIENT_SOURCE: LazyLock<reqwest::Client> =
    LazyLock::new(build_source_client);

/// Dedicated HTTP client for playback (initial GET + Range restarts).
/// Separate from HTTP_CLIENT; playback gets its own TCP connection,
/// avoiding HTTP/2 bandwidth contention with preload downloads.
pub static HTTP_CLIENT_PLAYBACK: LazyLock<MediaClient> =
    LazyLock::new(|| build_media_client(MEDIA_SILENCE));

/// Preload's own pool, so a listener's stream never queues behind staged traffic.
pub static HTTP_CLIENT_PRELOAD: LazyLock<MediaClient> =
    LazyLock::new(|| build_media_client(MEDIA_SILENCE));

#[cfg(test)]
#[path = "../../tests/unit/state/net.rs"]
mod tests;
