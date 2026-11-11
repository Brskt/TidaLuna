//! Serves animated covers to the page over a scheme we own, in three routes named by the host:
//!
//! * `video/?src=&edge=`: the cover transcoded to VP9 WebM, from the disk cache, with ranges. The
//!   page points TIDAL's own `<video>` at it. 404 until it is transcoded.
//! * `frames/?src=&edge=`: the cover's decoded I420 frames, one pass, for the page to transcode.
//!   Neither `cefQuery` nor `execute_java_script` can carry them, both marshalling through text.
//! * `store/?src=&edge=` (POST): the transcoded WebM, validated before it reaches the cache.
//!
//! A registered scheme rather than a synthetic `https://` path: `store_proxy` and `luna_modules`
//! both bet that their path never collides with a real TIDAL request, and a scheme we own does
//! not take that bet.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crate::ui::nav::PageKind;
use crate::video::cover::CoverKey;

use frames::FrameBody;

mod frames;
mod handler;
mod store;
mod video;

pub(crate) use handler::register;

pub(crate) const COVER_SCHEME: &str = "luna-cover";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CoverRoute {
    Video,
    Frames,
    Store,
}

impl CoverRoute {
    fn method(self) -> &'static str {
        match self {
            Self::Video | Self::Frames => "GET",
            Self::Store => "POST",
        }
    }
}

/// Refused before any work unless both route and cover are ours. The source is a percent-encoded
/// query parameter, not a path: the `https://` URL's slashes and query survive the round trip.
/// Other parameters (the page's cache-busting `r`) are ignored.
fn parse_request(url: &str) -> Result<(CoverRoute, CoverKey), &'static str> {
    let parsed = url::Url::parse(url).map_err(|_| "the cover request does not parse")?;
    let route = match parsed.host_str() {
        Some("video") => CoverRoute::Video,
        Some("frames") => CoverRoute::Frames,
        Some("store") => CoverRoute::Store,
        _ => return Err("the cover request names no known route"),
    };
    let (mut src, mut edge) = (None, None);
    for (name, value) in parsed.query_pairs() {
        match &*name {
            "src" => src = Some(value.into_owned()),
            "edge" => edge = Some(value.into_owned()),
            _ => {}
        }
    }
    let src = src.ok_or("the cover request carries no src")?;
    let edge = edge.ok_or("the cover request carries no edge")?;
    Ok((route, CoverKey::parse(&src, &edge)?))
}

/// One request's answer, as `response_headers` and `read` serve it.
#[derive(Default)]
struct Answer {
    status: u16,
    reason: String,
    mime: &'static str,
    body: Vec<u8>,
    offset: usize,
    accept_ranges: bool,
    content_range: Option<String>,
    /// Where `body` starts in the cover when it is the part a range asked for.
    first_byte: Option<usize>,
}

/// What `skip` tells CEF, which applies a `Range` by skipping to its first byte before it asks
/// for the headers.
#[derive(Debug, PartialEq, Eq)]
enum SkipVerdict {
    Skipped(i64),
    Refused,
}

impl Answer {
    fn refusal(status: u16, reason: impl Into<String>) -> Self {
        Self {
            status,
            reason: reason.into(),
            ..Self::default()
        }
    }

    /// Acknowledged without moving, and only over a body that already starts at `n`: Chromium
    /// places a range's body at its first byte without checking our status or `Content-Range`.
    /// Zero is never acknowledged; CEF would then wait for a callback that never comes.
    fn skip(&self, n: i64) -> SkipVerdict {
        match self.first_byte {
            Some(start) if n > 0 && usize::try_from(n) == Ok(start) => SkipVerdict::Skipped(n),
            _ => SkipVerdict::Refused,
        }
    }
}

/// What a handler shares with the work it started.
///
/// `cancelled` is an atomic outside both locks: `cancel()` runs on the IO thread and must never
/// wait on a frame being decoded under `frames`, nor the work on `answer`.
#[derive(Default)]
struct CoverShared {
    answer: Mutex<Answer>,
    /// The `frames/` body, decoded one frame per `read`. Its own lock, held only by `read`.
    frames: Mutex<Option<FrameBody>>,
    streaming: AtomicBool,
    cancelled: AtomicBool,
}

/// Why a requester is refused, or `None` to serve it.
///
/// The work behind this scheme runs in the browser process, and any document loaded in the main
/// browser can issue the request: an External iframe or page would otherwise make us download and
/// decode on its behalf. Only TIDAL's own pages may start it, by the rule privileged IPC applies.
/// The frame is CEF's, never the page's word; a frameless request (a service worker's) is refused.
fn requester_refusal(frame_url: Option<&str>) -> Option<&'static str> {
    match frame_url {
        None => Some("the cover request has no requesting frame"),
        Some(url) if !PageKind::classify(url).is_trusted() => {
            Some("the requesting frame is not a TIDAL page")
        }
        Some(_) => None,
    }
}

/// A fixed number of slots, each held by the value that took it until that value drops.
struct SlotPool {
    used: AtomicUsize,
    max: usize,
}

struct Slot(&'static SlotPool);

impl SlotPool {
    const fn new(max: usize) -> Self {
        Self {
            used: AtomicUsize::new(0),
            max,
        }
    }

    fn try_acquire(&'static self) -> Option<Slot> {
        self.used
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < self.max).then_some(n + 1)
            })
            .ok()
            .map(|_| Slot(self))
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.used.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Decoding streams at once. The page transcodes one cover at a time; the cap is for any other
/// page script, which could otherwise open a decode per request. Each stream also holds a decoder
/// and its cover's samples.
static FRAME_STREAMS: SlotPool = SlotPool::new(2);

/// What a finished `frames/` pass lets the page store: the size and timing its WebM must have.
#[derive(Clone, Copy, Debug)]
struct Grant {
    width: usize,
    height: usize,
    frames: usize,
    span_micros: i64,
    at: Instant,
}

impl Grant {
    /// What the WebM must carry. Its duration is the last frame's time plus one frame, as the
    /// page's muxer writes it, and it may be off by one frame either side.
    fn expected(&self) -> crate::video::webm::ExpectedCover {
        let frame_micros = match self.frames {
            0 | 1 => 0,
            n => self.span_micros / (n as i64 - 1),
        };
        crate::video::webm::ExpectedCover {
            width: self.width,
            height: self.height,
            duration_micros: self.span_micros + frame_micros,
            tolerance_micros: frame_micros + 1_000,
        }
    }
}

/// How long a pass's grant waits for its store: the page encodes after the last frame, and how
/// long that takes depends on the machine.
const GRANT_LIFETIME: Duration = Duration::from_secs(120);

/// Grants alive at once. Each is a few words; the bound only stops a page that reads passes and
/// never stores from growing the map.
const MAX_GRANTS: usize = 16;

/// The covers the page may store, one store per finished `frames/` pass.
///
/// A store with no pass behind it is refused: the size and duration a WebM is checked against
/// come from the decode, never from the page that posts it. A grant is opened at the pass's last
/// frame, and nothing about its request later (a cancel, the handler dropping) closes it.
#[derive(Default)]
struct StoreGrants {
    map: HashMap<CoverKey, Grant>,
}

impl StoreGrants {
    fn insert(&mut self, key: CoverKey, grant: Grant) {
        self.map
            .retain(|_, g| grant.at.saturating_duration_since(g.at) <= GRANT_LIFETIME);
        if self.map.len() >= MAX_GRANTS
            && !self.map.contains_key(&key)
            && let Some(oldest) = self
                .map
                .iter()
                .min_by_key(|(_, g)| g.at)
                .map(|(k, _)| k.clone())
        {
            self.map.remove(&oldest);
        }
        self.map.insert(key, grant);
    }

    /// The key's grant, used up, unless it expired.
    fn take(&mut self, key: &CoverKey, now: Instant) -> Option<Grant> {
        self.map
            .remove(key)
            .filter(|g| now.saturating_duration_since(g.at) <= GRANT_LIFETIME)
    }
}

static GRANTS: std::sync::LazyLock<Mutex<StoreGrants>> =
    std::sync::LazyLock::new(|| Mutex::new(StoreGrants::default()));

#[cfg(test)]
#[path = "../../../tests/unit/ui/cover_scheme.rs"]
mod tests;
