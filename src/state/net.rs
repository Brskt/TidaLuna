use std::sync::{Arc, LazyLock};
use std::time::Duration;

use crate::plugins::outbound::OutboundRequest;
use crate::ui::hosts::{EgressRefusal, HostShape, HostVerdict, Name, PermittedUrl};

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
#[expect(
    clippy::disallowed_methods,
    reason = "the one place a client builder starts"
)]
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

fn build_core_client() -> reqwest::Client {
    base_client_builder()
        .build()
        .expect("failed to build core HTTP client")
}

/// How long a media server may stay silent: no headers after a send, or no frame while a read
/// waits. 15 s clears the 8 s connect budget with room for a cold CDN edge's first byte.
const MEDIA_SILENCE: Duration = Duration::from_secs(15);

/// No `read_timeout` here: reqwest 0.13.5 counts the time between two reads against it, and the
/// download loops pause there on purpose for the governor. [`MediaBody::next`] bounds each read.
/// Its redirects stay in the plugin class: a media URL comes from TIDAL's answer or from a plugin.
fn build_media_client(silence: Duration) -> MediaClient {
    let inner = host_checked_builder(plugin_class, PLUGIN_REDIRECT_REFUSED, HopCap::Error)
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
            client: self.inner.clone(),
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
    /// The builder's own client, which sends the request once its URL has been judged.
    client: reqwest::Client,
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

    /// Times out on a server that accepts the connection and never answers, redirects included.
    /// A media URL, from TIDAL's answer or a plugin, is never sent when it names a sign-in host.
    pub async fn send(self) -> Result<MediaResponse, AudioFetchError> {
        let request = self.inner.build().map_err(AudioFetchError::transport)?;
        if let Some(refusal) = HostVerdict::of(request.url()).refusal() {
            return Err(AudioFetchError::Refused(refusal));
        }
        match tokio::time::timeout(self.silence, self.client.execute(request)).await {
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
    /// Never sent: the URL's host is one no media request may reach.
    Refused(EgressRefusal),
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
            Self::Refused(refusal) => refusal.fmt(f),
        }
    }
}

impl std::error::Error for AudioFetchError {}

/// The client for plugin sources, in the plugin class. Its `read_timeout` ends a source that stops
/// sending, a bound [`PluginClient`] cannot take since it also carries `plugin.fetch`. Its own jar
/// keeps a source's redirect cookies apart from those of `plugin.fetch`.
fn build_source_client() -> reqwest::Client {
    host_checked_builder(plugin_class, PLUGIN_REDIRECT_REFUSED, HopCap::Error)
        .cookie_store(true)
        // From send() to the headers too, redirects included; 30 s is what the whole fetch had.
        .read_timeout(Duration::from_secs(30))
        .build()
        .expect("failed to build plugin source HTTP client")
}

/// Whether a native plugin's `net.fetch` follows redirects. `redirect: "manual"` and `"error"`
/// take [`Hops::Never`], which hands the 3xx back to the plugin.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Hops {
    /// Within the plugin class, like every client a plugin drives.
    Follow,
    Never,
}

/// A native-plugin egress client with a caller-owned cookie jar, for per-plugin isolation.
pub(crate) fn build_native_client(jar: Arc<reqwest::cookie::Jar>, hops: Hops) -> reqwest::Client {
    let builder = match hops {
        Hops::Follow => host_checked_builder(plugin_class, PLUGIN_REDIRECT_REFUSED, HopCap::Error),
        Hops::Never => base_client_builder().redirect(reqwest::redirect::Policy::none()),
    };
    builder
        .cookie_provider(jar)
        .build()
        .expect("failed to build native HTTP client")
}

/// Redirects one request may follow after its first URL.
const MAX_REDIRECT_HOPS: usize = 10;

/// What a host-checked client does with a redirect past [`MAX_REDIRECT_HOPS`].
#[derive(Debug, Clone, Copy)]
pub(crate) enum HopCap {
    /// Hands back the last 3xx, for the caller's status check to reject.
    Stop,
    /// Fails the request with [`TooManyRedirects`], which is no refusal: a 403 would misname it.
    Error,
}

/// A redirect chain longer than [`MAX_REDIRECT_HOPS`].
#[derive(Debug)]
pub(crate) struct TooManyRedirects;

impl std::fmt::Display for TooManyRedirects {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "more than {MAX_REDIRECT_HOPS} redirects")
    }
}

impl std::error::Error for TooManyRedirects {}

/// A builder whose client re-checks `allowed` at every redirect hop and refuses with
/// [`crate::util::Refused`]`(refusal)` the moment a hop leaves it. An open redirect on an allowed
/// host would otherwise carry the request anywhere. The first URL is the caller's to judge.
pub(crate) fn host_checked_builder(
    allowed: fn(&url::Url) -> bool,
    refusal: &'static str,
    cap: HopCap,
) -> reqwest::ClientBuilder {
    base_client_builder().redirect(reqwest::redirect::Policy::custom(move |attempt| {
        if !allowed(attempt.url()) {
            // The host alone: the rest of a redirect URL can carry a signature or a code.
            crate::verr!(
                "[NET] redirect refused to host {}",
                crate::util::refused_host(attempt.url().as_str())
            );
            return attempt.error(crate::util::Refused(refusal));
        }
        // `previous` holds the first URL and every hop followed so far.
        if attempt.previous().len() > MAX_REDIRECT_HOPS {
            return match cap {
                HopCap::Stop => attempt.stop(),
                HopCap::Error => attempt.error(TooManyRedirects),
            };
        }
        attempt.follow()
    }))
}

/// [`host_checked_builder`] with no cookie store, stopping at the cap.
pub(crate) fn build_host_checked_client(
    allowed: fn(&url::Url) -> bool,
    refusal: &'static str,
) -> reqwest::Client {
    host_checked_builder(allowed, refusal, HopCap::Stop)
        .build()
        .expect("failed to build redirect-checked HTTP client")
}

const PLUGIN_REDIRECT_REFUSED: &str = "redirect reached a host plugins may not reach";

/// Every host a plugin may reach: anything but a TIDAL sign-in or malformed host.
pub(crate) fn plugin_class(url: &url::Url) -> bool {
    HostVerdict::of(url) == HostVerdict::Open
}

/// TIDAL's token host. One test, shared by the token client and Connect's token root.
pub(crate) fn is_token_host(name: Name<'_>) -> bool {
    name.is(crate::ui::nav::HOST_AUTH)
}

/// TIDAL's service hosts. One test, shared by the service client and Connect's service roots.
pub(crate) fn is_service_host(name: Name<'_>) -> bool {
    crate::ui::nav::SERVICE_HOSTS
        .iter()
        .any(|host| name.is(host))
}

fn https_on(url: &url::Url, host: fn(Name<'_>) -> bool) -> bool {
    url.scheme() == "https" && matches!(HostShape::of(url), HostShape::Name(name) if host(name))
}

/// HTTPS on TIDAL's token host alone.
pub(crate) fn token_class(url: &url::Url) -> bool {
    https_on(url, is_token_host)
}

/// HTTPS on one of TIDAL's service hosts.
pub(crate) fn service_class(url: &url::Url) -> bool {
    https_on(url, is_service_host)
}

/// The client every plugin request goes through: the plugin class at every hop, one shared jar.
/// It takes only a [`PermittedUrl`]: a plugin's first URL is judged before anything is sent.
pub(crate) struct PluginClient(reqwest::Client);

impl PluginClient {
    fn build() -> Self {
        Self(
            host_checked_builder(plugin_class, PLUGIN_REDIRECT_REFUSED, HopCap::Error)
                .cookie_store(true)
                .build()
                .expect("failed to build plugin HTTP client"),
        )
    }

    /// An admitted plugin request, every field as the plugin sent it.
    pub(crate) fn send(&self, request: OutboundRequest) -> reqwest::RequestBuilder {
        request.build(&self.0)
    }

    pub(crate) fn request(
        &self,
        method: reqwest::Method,
        url: PermittedUrl,
    ) -> reqwest::RequestBuilder {
        self.0.request(method, url.into_url())
    }
}

/// Requests Rust makes for itself (the updater). No cookie store: nothing it reaches keeps a session.
pub static CORE_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(build_core_client);

/// Rust's own refreshes, in the token class. No cookie store.
pub(crate) static AUTH_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    host_checked_builder(
        token_class,
        "redirect left TIDAL's token host",
        HopCap::Error,
    )
    .build()
    .expect("failed to build auth HTTP client")
});

/// Connect's requests to the queue and content servers, in the service class. No cookie store.
pub(crate) static SERVICE_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    host_checked_builder(
        service_class,
        "redirect left TIDAL's service hosts",
        HopCap::Error,
    )
    .build()
    .expect("failed to build service HTTP client")
});

pub(crate) static PLUGIN_CLIENT: LazyLock<PluginClient> = LazyLock::new(PluginClient::build);

pub(crate) static HTTP_CLIENT_SOURCE: LazyLock<reqwest::Client> =
    LazyLock::new(build_source_client);

/// Dedicated HTTP client for playback (initial GET + Range restarts).
/// Separate from the other clients; playback gets its own TCP connection,
/// avoiding HTTP/2 bandwidth contention with preload downloads.
pub static HTTP_CLIENT_PLAYBACK: LazyLock<MediaClient> =
    LazyLock::new(|| build_media_client(MEDIA_SILENCE));

/// Preload's own pool, so a listener's stream never queues behind staged traffic.
pub static HTTP_CLIENT_PRELOAD: LazyLock<MediaClient> =
    LazyLock::new(|| build_media_client(MEDIA_SILENCE));

#[cfg(test)]
#[path = "../../tests/unit/state/net.rs"]
mod tests;
