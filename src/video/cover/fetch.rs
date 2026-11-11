use crate::video::codec::FailureClass;

/// Covers are a few megabytes. The ceiling bounds a body whose length the server declares and
/// we do not trust.
const MAX_COVER_BYTES: usize = 32 * 1024 * 1024;

/// One small file from a CDN. `base_client_builder` sets no read timeout on purpose, so this
/// path sets its own.
const COVER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// An animated cover, on TIDAL's artwork host, under the video path.
///
/// The host test is the exact equality `ipc::plugin::download` applies to artwork, never a
/// substring. The path check is ours: the same host serves the static `images/` covers, which
/// must never reach a decoder. A port or a user part names another service or another identity
/// than the CDN's, and the URL is a cache key besides: both are refused.
fn is_cover_url(url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    parsed.scheme() == "https"
        && parsed.host_str() == Some("resources.tidal.com")
        && parsed.port().is_none()
        && parsed.username().is_empty()
        && parsed.password().is_none()
        && parsed.path().starts_with("/videos/")
}

/// The square sizes TIDAL publishes for an animated cover, in pixels on a side.
const SERVED_EDGES: [u32; 5] = [80, 160, 320, 640, 1280];

/// The smallest published size that covers what the page asked for, or the largest one.
fn served_edge(asked: u32) -> u32 {
    SERVED_EDGES
        .into_iter()
        .find(|&edge| edge >= asked)
        .unwrap_or(SERVED_EDGES[SERVED_EDGES.len() - 1])
}

/// One cover at one size: what is downloaded, decoded, transcoded and cached.
///
/// Built only by [`CoverKey::parse`] from the page's `src`: a cover URL ending in a square
/// `<n>x<n>.mp4`, rewritten to the served edge, query and fragment dropped. A page cannot mint
/// cache entries for one cover by varying them.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct CoverKey {
    url: String,
}

impl CoverKey {
    pub(crate) fn parse(src: &str, edge: &str) -> Result<Self, &'static str> {
        if !is_cover_url(src) {
            return Err("the cover url is not a TIDAL artwork video url");
        }
        let asked: u32 = edge
            .parse()
            .ok()
            .filter(|&e| e > 0)
            .ok_or("the cover edge is not a positive integer")?;
        let edge = served_edge(asked);

        let mut parsed = url::Url::parse(src).map_err(|_| "the cover url does not parse")?;
        parsed.set_query(None);
        parsed.set_fragment(None);
        let last = parsed
            .path_segments()
            .and_then(|mut segments| segments.next_back())
            .ok_or("the cover url has no file name")?;
        let (width, height) = last
            .strip_suffix(".mp4")
            .and_then(|size| size.split_once('x'))
            .ok_or("the cover url names no <n>x<n>.mp4 variant")?;
        if width.parse::<u32>().is_err() || width != height {
            return Err("the cover url names no square variant");
        }
        parsed
            .path_segments_mut()
            .map_err(|_| "the cover url cannot carry a path")?
            .pop()
            .push(&format!("{edge}x{edge}.mp4"));
        Ok(Self {
            url: parsed.to_string(),
        })
    }

    pub(crate) fn url(&self) -> &str {
        &self.url
    }
}

/// The cover fetch's own client. Not `HTTP_CLIENT`: it follows redirects with no policy, so an
/// open redirect on the artwork host would carry the fetch anywhere, and it shares a cookie jar.
/// This one re-checks the host at every hop, as `ipc::plugin::download` does.
static COVER_CLIENT: std::sync::LazyLock<reqwest::Client> = std::sync::LazyLock::new(|| {
    crate::state::build_host_checked_client(
        cover_redirect_allowed,
        "redirect left the artwork host",
    )
});

/// The artwork host, over HTTPS. Not `is_cover_url`: a hop need not stay under `/videos/`.
fn cover_redirect_allowed(url: &url::Url) -> bool {
    url.scheme() == "https" && url.host_str() == Some("resources.tidal.com")
}

/// Why a cover download failed, tagged by the stage that failed, as the codec's `FetchError` is.
/// No `From`: a new step cannot fall by default into the class the page retries.
#[derive(Debug)]
pub(crate) enum CoverFetchError {
    /// Not a cover on the artwork host; `CoverKey::parse` refused it first.
    NotACover,
    /// Anything before a status line: DNS, connect, TLS, the timeout.
    Unreachable(reqwest::Error),
    /// A redirect the host-checked client refused.
    Redirected(reqwest::Error),
    Status(reqwest::StatusCode),
    /// The body broke off after the status line.
    Body(reqwest::Error),
    TooLarge,
}

impl CoverFetchError {
    /// Read off the stage, never off the text.
    pub(crate) fn class(&self) -> FailureClass {
        match self {
            Self::Unreachable(_) | Self::Body(_) => FailureClass::Network,
            Self::Status(s) => FailureClass::of_status(*s),
            Self::NotACover | Self::Redirected(_) => FailureClass::Refused,
            Self::TooLarge => FailureClass::Corrupt,
        }
    }
}

/// The whole cause, for the log only: the page is told the class.
impl std::fmt::Display for CoverFetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use crate::video::codec::write_chain;
        match self {
            Self::NotACover => f.write_str("cover url is not a TIDAL artwork video url"),
            Self::Unreachable(e) => write_chain(f, "sending the cover request failed", e),
            Self::Redirected(e) => write_chain(f, "the artwork host redirected elsewhere", e),
            Self::Status(s) => write!(f, "artwork host answered {s}"),
            Self::Body(e) => write_chain(f, "reading the cover failed", e),
            Self::TooLarge => write!(f, "cover exceeds the ceiling of {MAX_COVER_BYTES} bytes"),
        }
    }
}

/// No `source()`: `Display` already carries the cause.
impl std::error::Error for CoverFetchError {}

/// Fetch a cover whole, or stop early and return `None` once its request is cancelled.
///
/// Small enough to hold in memory, and the demuxer wants it that way: it does no IO of its own and
/// is documented as accepting the entire file in one call.
pub(crate) async fn fetch_cover(
    url: &str,
    cancelled: &std::sync::atomic::AtomicBool,
) -> Result<Option<Vec<u8>>, CoverFetchError> {
    if !is_cover_url(url) {
        crate::vprintln!(
            "[VIDEO]   Refused a cover url outside the artwork host: {}",
            crate::util::refused_host(url)
        );
        return Err(CoverFetchError::NotACover);
    }

    let resp = COVER_CLIENT
        .get(url)
        .timeout(COVER_TIMEOUT)
        .send()
        .await
        .map_err(|e| {
            // The policy's refusal surfaces as a send error too; only this flag tells it apart
            // from a host that never answered.
            if e.is_redirect() {
                CoverFetchError::Redirected(e.without_url())
            } else {
                CoverFetchError::Unreachable(e.without_url())
            }
        })?;
    if !resp.status().is_success() {
        return Err(CoverFetchError::Status(resp.status()));
    }

    let mut out = Vec::new();
    let mut stream = resp.bytes_stream();
    {
        use futures_util::StreamExt;
        while let Some(chunk) = stream.next().await {
            // Asked once per chunk, a boundary where stopping is free; without it an abandoned
            // cover downloads to its last byte, or to `COVER_TIMEOUT`.
            if cancelled.load(std::sync::atomic::Ordering::Relaxed) {
                return Ok(None);
            }
            let chunk = chunk.map_err(|e| CoverFetchError::Body(e.without_url()))?;
            if out.len() + chunk.len() > MAX_COVER_BYTES {
                return Err(CoverFetchError::TooLarge);
            }
            out.extend_from_slice(&chunk);
        }
    }
    Ok(Some(out))
}

#[cfg(test)]
#[path = "../../../tests/unit/video/cover/fetch.rs"]
mod tests;
