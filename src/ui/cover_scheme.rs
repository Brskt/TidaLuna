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
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cef::*;

use crate::ui::nav::PageKind;
use crate::ui::token_filter::userfree_to_string;
use crate::video::codec::FailureClass;
use crate::video::cover::{CoverFrame, CoverKey, DecodeStream};

pub(crate) const COVER_SCHEME: &str = "luna-cover";

/// Magic naming the frames wire format: a page handed a foreign body says so instead of encoding
/// noise.
const MAGIC: &[u8; 4] = b"LVC2";

/// Bytes before the first frame: the magic, then width, height and frame count as u32.
const HEADER_LEN: usize = 16;

/// Bytes of the record in front of each frame's planes: pts (i64), three strides and three plane
/// lengths (u32).
const RECORD_LEN: usize = 32;

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

/// Hand one request its answer and resume CEF on its IO thread, where `cancel()` also runs: the
/// `cancelled` check before `cont()` cannot then race a cancel.
fn deliver(shared: Arc<CoverShared>, callback: Callback, answer: Answer) {
    *shared.answer.lock().unwrap_or_else(|e| e.into_inner()) = answer;
    let mut task = CoverContinueTask::new(callback, shared);
    post_task(ThreadId::IO, Some(&mut task));
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

/// The `frames/` body: the header, then each frame's record and planes, decoded as `read` asks.
struct FrameBody {
    stream: DecodeStream,
    key: CoverKey,
    buf: Vec<u8>,
    offset: usize,
    done: bool,
    _slot: Slot,
}

impl FrameBody {
    fn new(stream: DecodeStream, key: CoverKey, slot: Slot) -> Self {
        let (width, height) = stream.size();
        let mut buf = Vec::with_capacity(HEADER_LEN);
        buf.extend_from_slice(MAGIC);
        buf.extend_from_slice(&(width as u32).to_le_bytes());
        buf.extend_from_slice(&(height as u32).to_le_bytes());
        buf.extend_from_slice(&(stream.frame_count() as u32).to_le_bytes());
        Self {
            stream,
            key,
            buf,
            offset: 0,
            done: false,
            _slot: slot,
        }
    }

    /// Copy the next bytes into `out`, decoding a frame when the one in hand is spent. Zero at
    /// the end, when the pass's grant is opened.
    fn fill(&mut self, out: &mut [u8]) -> anyhow::Result<usize> {
        loop {
            let left = &self.buf[self.offset..];
            if !left.is_empty() {
                let n = left.len().min(out.len());
                out[..n].copy_from_slice(&left[..n]);
                self.offset += n;
                return Ok(n);
            }
            if self.done {
                return Ok(0);
            }
            match self.stream.next_frame()? {
                Some(frame) => {
                    encode_frame(&frame, &mut self.buf);
                    self.offset = 0;
                }
                None => {
                    self.done = true;
                    let (width, height) = self.stream.size();
                    GRANTS.lock().unwrap_or_else(|e| e.into_inner()).insert(
                        self.key.clone(),
                        Grant {
                            width,
                            height,
                            frames: self.stream.frame_count(),
                            span_micros: self.stream.span_micros(),
                            at: Instant::now(),
                        },
                    );
                    crate::vprintln!(
                        "[VIDEO]   Cover frames sent ({} at {width}x{height})",
                        self.stream.frame_count()
                    );
                }
            }
        }
    }
}

/// One frame's record and planes, over `buf`'s previous contents.
fn encode_frame(frame: &CoverFrame, buf: &mut Vec<u8>) {
    buf.clear();
    buf.reserve(RECORD_LEN + frame.y.len() + frame.u.len() + frame.v.len());
    buf.extend_from_slice(&frame.pts_micros.to_le_bytes());
    for value in [
        frame.y_stride,
        frame.u_stride,
        frame.v_stride,
        frame.y.len(),
        frame.u.len(),
        frame.v.len(),
    ] {
        buf.extend_from_slice(&(value as u32).to_le_bytes());
    }
    buf.extend_from_slice(&frame.y);
    buf.extend_from_slice(&frame.u);
    buf.extend_from_slice(&frame.v);
}

/// The part of a `video/` file one `Range` header asks for.
enum RangeAnswer {
    Whole,
    /// Inclusive bounds, as `Content-Range` writes them.
    Part {
        start: usize,
        end: usize,
    },
    Unsatisfiable,
}

/// One `bytes=` range of RFC 9110, section 14.1.2. Anything else (several ranges, another unit,
/// a malformed value) is answered whole, which a server is allowed to do.
fn resolve_range(header: Option<&str>, len: usize) -> RangeAnswer {
    let Some(spec) = header.and_then(|h| h.trim().strip_prefix("bytes=")) else {
        return RangeAnswer::Whole;
    };
    if spec.contains(',') {
        return RangeAnswer::Whole;
    }
    let Some((first, last)) = spec.trim().split_once('-') else {
        return RangeAnswer::Whole;
    };
    let (first, last) = (first.trim(), last.trim());
    if first.is_empty() {
        // A suffix: the last N bytes.
        return match last.parse::<usize>() {
            Ok(0) => RangeAnswer::Unsatisfiable,
            Ok(_) if len == 0 => RangeAnswer::Unsatisfiable,
            Ok(n) => RangeAnswer::Part {
                start: len - n.min(len),
                end: len - 1,
            },
            Err(_) => RangeAnswer::Whole,
        };
    }
    let Ok(start) = first.parse::<usize>() else {
        return RangeAnswer::Whole;
    };
    let end = if last.is_empty() {
        usize::MAX
    } else {
        match last.parse::<usize>() {
            Ok(end) if end >= start => end,
            _ => return RangeAnswer::Whole,
        }
    };
    if start >= len {
        return RangeAnswer::Unsatisfiable;
    }
    RangeAnswer::Part {
        start,
        end: end.min(len - 1),
    }
}

/// `video/`: the cached WebM, or why there is none.
fn video_answer(key: &CoverKey, range: Option<&str>) -> Answer {
    match crate::video::cover_cache::COVER_CACHE.get(key) {
        Some(data) => ranged_answer(data, range),
        None => Answer::refusal(404, "not transcoded"),
    }
}

fn ranged_answer(mut data: Vec<u8>, range: Option<&str>) -> Answer {
    let len = data.len();
    let mut answer = Answer {
        status: 200,
        mime: "video/webm",
        accept_ranges: true,
        ..Answer::default()
    };
    match resolve_range(range, len) {
        RangeAnswer::Whole => {}
        RangeAnswer::Part { start, end } => {
            data.truncate(end + 1);
            data.drain(..start);
            answer.status = 206;
            answer.content_range = Some(format!("bytes {start}-{end}/{len}"));
            answer.first_byte = Some(start);
        }
        RangeAnswer::Unsatisfiable => {
            return Answer {
                status: 416,
                reason: "the range lies outside the cover".to_string(),
                accept_ranges: true,
                content_range: Some(format!("bytes */{len}")),
                ..Answer::default()
            };
        }
    }
    answer.body = data;
    answer
}

/// The total a POST body's elements add up to, refused past `max` before anything is copied: the
/// sizes are the page's, and a sum computed first is what decides whether to allocate.
fn body_size(
    sizes: impl IntoIterator<Item = usize>,
    max: usize,
) -> Result<usize, (u16, &'static str)> {
    let mut total: usize = 0;
    for size in sizes {
        total = total
            .checked_add(size)
            .filter(|&t| t <= max)
            .ok_or((413, "the upload is larger than a cover may be"))?;
    }
    if total == 0 {
        return Err((400, "the upload is empty"));
    }
    Ok(total)
}

/// Only bytes elements carry anything here: CEF hands a scheme handler a `Blob` or stream body as
/// an empty element list, which is why the page posts a typed array. Kept apart from
/// `token_filter`'s reader, which has no cap and must keep answering as it does.
fn read_capped_body(request: &mut Request, max: usize) -> Result<Vec<u8>, (u16, &'static str)> {
    let post_data = request.post_data().ok_or((400, "the upload is empty"))?;
    let count = post_data.element_count();
    let mut elements: Vec<Option<PostDataElement>> = vec![None; count];
    post_data.elements(Some(&mut elements));
    let total = body_size(elements.iter().flatten().map(|el| el.bytes_count()), max)?;
    let mut body = Vec::with_capacity(total);
    for el in elements.iter().flatten() {
        let n = el.bytes_count();
        if n == 0 {
            continue;
        }
        let start = body.len();
        body.resize(start + n, 0);
        let copied = el.bytes(n, body[start..].as_mut_ptr());
        body.truncate(start + copied.min(n));
    }
    Ok(body)
}

/// `store/`: the WebM checked against the grant its pass opened, then cached.
fn store_answer(key: &CoverKey, body: &[u8]) -> Answer {
    let grant = GRANTS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take(key, Instant::now());
    let Some(grant) = grant else {
        return Answer::refusal(409, "no transcode of this cover is awaiting its upload");
    };
    if let Err(why) = crate::video::webm::validate_cover_webm(body, &grant.expected()) {
        crate::verr!("[VIDEO]   Refused a transcoded cover: {why}");
        return Answer::refusal(422, why);
    }
    match crate::video::cover_cache::COVER_CACHE.put(key, body) {
        Ok(()) => {
            crate::vprintln!(
                "[VIDEO]   Cover transcoded and stored ({} bytes)",
                body.len()
            );
            Answer {
                status: 200,
                ..Answer::default()
            }
        }
        Err(e) => {
            crate::verr!("[VIDEO]   Storing a transcoded cover failed: {e}");
            let (status, reason) = put_refusal(&e);
            Answer::refusal(status, reason)
        }
    }
}

/// A full disk heals, with time or with the user: the page asks again later. Any other write
/// failure says nothing of the next one.
fn put_refusal(e: &std::io::Error) -> (u16, &'static str) {
    match e.kind() {
        std::io::ErrorKind::StorageFull | std::io::ErrorKind::QuotaExceeded => {
            (507, "the disk has no room for the cover")
        }
        _ => (500, "the cover could not be stored"),
    }
}

/// Why `frames/` could not open, by stage.
enum FramesError {
    Download(crate::video::cover::CoverFetchError),
    Load(anyhow::Error),
    Open(crate::video::cover::OpenError),
    Panicked(tokio::task::JoinError),
}

impl FramesError {
    fn class(&self) -> FailureClass {
        match self {
            Self::Download(e) => e.class(),
            Self::Load(_) => FailureClass::Unloadable,
            Self::Open(e) => e.class(),
            Self::Panicked(_) => FailureClass::Crashed,
        }
    }
}

impl std::fmt::Display for FramesError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Download(e) => write!(f, "downloading the cover failed: {e}"),
            Self::Load(e) => write!(f, "loading the codec failed: {e:#}"),
            Self::Open(e) => write!(f, "decoding the cover failed: {e}"),
            Self::Panicked(e) => write!(f, "the cover decode task failed: {e}"),
        }
    }
}

/// The status tells the page what to do: 502 asks again soon (the network), 503 waits for the
/// codec, 422 and 500 spend an attempt (the cover, or a fault of ours).
fn frames_refusal(class: FailureClass) -> (u16, &'static str) {
    match class {
        FailureClass::Network => (502, "the cover could not be fetched right now"),
        FailureClass::Refused => (422, "the artwork host does not serve this cover"),
        FailureClass::Corrupt => (422, "the cover cannot be transcoded"),
        FailureClass::Unloadable => (503, "the video codec could not be loaded"),
        FailureClass::Disk | FailureClass::Crashed => {
            (500, "the cover transcode failed inside the app")
        }
    }
}

/// `frames/`: the decoder, the file, and the first frame, before the headers go out.
async fn open_frames(shared: &CoverShared, key: &CoverKey) -> Result<DecodeStream, Answer> {
    let path = crate::video::codec::decoder().await.map_err(|why| {
        crate::vprintln!("[VIDEO]   Cover without a codec: {}", why.phrase());
        Answer::refusal(503, why.phrase())
    })?;
    let refusal = |e: FramesError| {
        // The cause stays in the log: a loader message names the codec's path on disk.
        crate::verr!("[VIDEO]   Cover frames failed: {e}");
        let (status, reason) = frames_refusal(e.class());
        Answer::refusal(status, reason)
    };
    let file = crate::video::cover::fetch_cover(key.url(), &shared.cancelled)
        .await
        .map_err(|e| refusal(FramesError::Download(e)))?
        // nginx's "client closed request".
        .ok_or_else(|| Answer::refusal(499, "the request was abandoned"))?;
    let opened = tokio::task::spawn_blocking(move || {
        let api = crate::video::codec::load_api(&path).map_err(FramesError::Load)?;
        DecodeStream::open(&file, api).map_err(FramesError::Open)
    })
    .await
    .unwrap_or_else(|e| Err(FramesError::Panicked(e)));
    opened.map_err(|e| {
        // The supervisor proved the codec loads and starts: one that no longer does changed since.
        if e.class() == FailureClass::Unloadable {
            crate::video::codec::recheck();
        }
        refusal(e)
    })
}

wrap_resource_handler! {
    pub(super) struct CoverHandler {
        shared: Arc<CoverShared>,
        // Decided in `create()`, where CEF names the requesting frame.
        refusal: Option<&'static str>,
    }

    impl ResourceHandler {
        fn open(
            &self,
            request: Option<&mut Request>,
            handle_request: Option<&mut ::std::os::raw::c_int>,
            callback: Option<&mut Callback>,
        ) -> ::std::os::raw::c_int {
            // Async path: return now and finish via `callback` once the work resolves.
            if let Some(hr) = handle_request {
                *hr = 0;
            }
            let Some(callback) = callback else {
                return 0;
            };
            let callback = callback.clone();
            let shared = self.shared.clone();
            let Some(request) = request else {
                deliver(shared, callback, Answer::refusal(400, "the cover request is missing"));
                return 1;
            };

            // Before anything that costs: no work starts for a refused requester.
            if let Some(reason) = self.refusal {
                deliver(shared, callback, Answer::refusal(403, reason));
                return 1;
            }
            let (route, key) = match parse_request(&userfree_to_string(&request.url())) {
                Ok(parsed) => parsed,
                Err(why) => {
                    deliver(shared, callback, Answer::refusal(400, why));
                    return 1;
                }
            };
            if userfree_to_string(&request.method()) != route.method() {
                deliver(shared, callback, Answer::refusal(405, "the cover route takes another method"));
                return 1;
            }

            match route {
                CoverRoute::Video => {
                    let range = userfree_to_string(
                        &request.header_by_name(Some(&CefString::from("Range"))),
                    );
                    crate::state::rt_handle().spawn(async move {
                        // The codec gates the cache too: a cover must not play with the switch off.
                        // Asked rather than read: at launch the installed codec is still being
                        // verified, and a cached cover refused then would be transcoded again.
                        if let Err(why) = crate::video::codec::decoder().await {
                            deliver(shared, callback, Answer::refusal(503, why.phrase()));
                            return;
                        }
                        let range = (!range.is_empty()).then_some(range);
                        let answer = tokio::task::spawn_blocking(move || {
                            video_answer(&key, range.as_deref())
                        })
                        .await
                        .unwrap_or_else(|_| Answer::refusal(500, "the cover could not be read"));
                        deliver(shared, callback, answer);
                    });
                }
                CoverRoute::Frames => {
                    let Some(slot) = FRAME_STREAMS.try_acquire() else {
                        deliver(shared, callback, Answer::refusal(429, "too many covers are being transcoded"));
                        return 1;
                    };
                    crate::state::rt_handle().spawn(async move {
                        let answer = match open_frames(&shared, &key).await {
                            Ok(stream) => {
                                *shared.frames.lock().unwrap_or_else(|e| e.into_inner()) =
                                    Some(FrameBody::new(stream, key, slot));
                                shared.streaming.store(true, Ordering::Release);
                                Answer {
                                    status: 200,
                                    mime: "application/octet-stream",
                                    ..Answer::default()
                                }
                            }
                            Err(answer) => answer,
                        };
                        deliver(shared, callback, answer);
                    });
                }
                CoverRoute::Store => {
                    let body = match read_capped_body(request, crate::video::webm::MAX_WEBM_BYTES) {
                        Ok(body) => body,
                        Err((status, why)) => {
                            deliver(shared, callback, Answer::refusal(status, why));
                            return 1;
                        }
                    };
                    crate::state::rt_handle().spawn(async move {
                        let answer =
                            tokio::task::spawn_blocking(move || store_answer(&key, &body))
                                .await
                                .unwrap_or_else(|_| {
                                    Answer::refusal(500, "the cover could not be stored")
                                });
                        deliver(shared, callback, answer);
                    });
                }
            }
            1
        }

        fn response_headers(
            &self,
            response: Option<&mut Response>,
            response_length: Option<&mut i64>,
            _redirect_url: Option<&mut CefString>,
        ) {
            let answer = self.shared.answer.lock().unwrap_or_else(|e| e.into_inner());
            let Some(resp) = response else {
                return;
            };
            let header = |resp: &mut Response, name: &str, value: &str| {
                resp.set_header_by_name(
                    Some(&CefString::from(name)),
                    Some(&CefString::from(value)),
                    1,
                );
            };
            // An answer never set is a fault of ours, never the network the page would retry.
            resp.set_status(if answer.status == 0 { 500 } else { answer.status as i32 });
            let mime = if answer.mime.is_empty() { "text/plain" } else { answer.mime };
            resp.set_mime_type(Some(&CefString::from(mime)));
            header(resp, "Access-Control-Allow-Origin", "*");
            // Exposed as well as allowed: a standard scheme has its own origin, which makes the
            // page's fetch cross-origin, and CORS shows it only safelisted headers. Without this,
            // every refusal would reach the page as a bare status code.
            header(resp, "Access-Control-Expose-Headers", "X-Luna-Cover-Reason");
            // The named reason travels as a header so the page can say why a cover stayed still
            // without a second round trip.
            if !answer.reason.is_empty() {
                header(resp, "X-Luna-Cover-Reason", &answer.reason);
            }
            if answer.accept_ranges {
                header(resp, "Accept-Ranges", "bytes");
            }
            if let Some(range) = &answer.content_range {
                header(resp, "Content-Range", range);
            }
            // A refusal is about this moment (the codec, the cache, the cap), never the cover.
            if answer.status >= 400 {
                header(resp, "Cache-Control", "no-store");
            }
            if let Some(len) = response_length {
                *len = if self.shared.streaming.load(Ordering::Acquire) {
                    // Decoded as it is read: the length is not known up front.
                    -1
                } else {
                    answer.body.len() as i64
                };
            }
        }

        fn read(
            &self,
            data_out: *mut u8,
            bytes_to_read: ::std::os::raw::c_int,
            bytes_read: Option<&mut ::std::os::raw::c_int>,
            _callback: Option<&mut ResourceReadCallback>,
        ) -> ::std::os::raw::c_int {
            let Some(br) = bytes_read else {
                return 0;
            };
            if bytes_to_read <= 0 || data_out.is_null() {
                *br = 0;
                return 0;
            }
            // SAFETY: CEF guarantees `data_out` is writable for at least `bytes_to_read` bytes for
            // the duration of this call, and nothing else holds it meanwhile.
            let out = unsafe { std::slice::from_raw_parts_mut(data_out, bytes_to_read as usize) };

            if self.shared.streaming.load(Ordering::Acquire) {
                // Decoded here, on the pool sequence CEF reads on: one frame is little work, and
                // nothing is parked waiting for one, which would hold `cancel()` back.
                let mut frames = self.shared.frames.lock().unwrap_or_else(|e| e.into_inner());
                let Some(body) = frames.as_mut() else {
                    *br = 0;
                    return 0;
                };
                if self.shared.cancelled.load(Ordering::Relaxed) {
                    *frames = None;
                    *br = 0;
                    return 0;
                }
                return match body.fill(out) {
                    Ok(0) => {
                        // Frees the decoder, the file and the stream's slot now, not at the
                        // handler's release.
                        *frames = None;
                        *br = 0;
                        0
                    }
                    Ok(n) => {
                        *br = n as ::std::os::raw::c_int;
                        1
                    }
                    Err(e) => {
                        crate::verr!("[VIDEO]   Cover frames stopped: {e}");
                        *frames = None;
                        // `ERR_FAILED`: the body ends in an error, never in a short success the
                        // page would take for the whole cover.
                        *br = -2;
                        0
                    }
                };
            }

            let mut answer = self.shared.answer.lock().unwrap_or_else(|e| e.into_inner());
            let offset = answer.offset;
            let left = answer.body.len().saturating_sub(offset);
            if left == 0 {
                *br = 0;
                return 0; // EOF
            }
            let n = left.min(out.len());
            out[..n].copy_from_slice(&answer.body[offset..offset + n]);
            answer.offset += n;
            *br = n as ::std::os::raw::c_int;
            1
        }

        fn skip(
            &self,
            bytes_to_skip: i64,
            bytes_skipped: Option<&mut i64>,
            _callback: Option<&mut ResourceSkipCallback>,
        ) -> ::std::os::raw::c_int {
            let Some(skipped) = bytes_skipped else {
                return 0;
            };
            let verdict = self
                .shared
                .answer
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .skip(bytes_to_skip);
            match verdict {
                SkipVerdict::Skipped(n) => {
                    *skipped = n;
                    1
                }
                // `ERR_FAILED`, as CEF's own default: a refused range is an error, never a
                // body placed at the wrong offset.
                SkipVerdict::Refused => {
                    *skipped = -2;
                    0
                }
            }
        }

        fn cancel(&self) {
            // Only the flag: the download stops at its next chunk, a decode at its next `read`,
            // and the stream and its slot go with the handler.
            self.shared.cancelled.store(true, Ordering::Relaxed);
        }
    }
}

wrap_task! {
    struct CoverContinueTask {
        callback: Callback,
        shared: Arc<CoverShared>,
    }
    impl Task {
        fn execute(&self) {
            if self.shared.cancelled.load(Ordering::Relaxed) {
                return;
            }
            self.callback.cont();
        }
    }
}

wrap_scheme_handler_factory! {
    pub(super) struct CoverSchemeFactory {}

    impl SchemeHandlerFactory {
        fn create(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            _scheme_name: Option<&CefString>,
            _request: Option<&mut Request>,
        ) -> Option<ResourceHandler> {
            let frame_url = frame.map(|f| userfree_to_string(&f.url()));
            let refusal = requester_refusal(frame_url.as_deref());
            if let Some(reason) = refusal {
                let from = frame_url
                    .as_deref()
                    .map_or_else(|| "no frame".to_string(), crate::util::refused_host);
                crate::vprintln!("[VIDEO]   Refused a cover request from {from}: {reason}");
            }
            Some(CoverHandler::new(Arc::new(CoverShared::default()), refusal))
        }
    }
}

/// Register the factory. Browser process only: `on_register_custom_schemes` runs in every
/// process, but a scheme handler factory only means anything where the network stack lives.
pub(crate) fn register() {
    let scheme = CefString::from(COVER_SCHEME);
    let mut factory = CoverSchemeFactory::new();
    if register_scheme_handler_factory(Some(&scheme), None, Some(&mut factory)) == 0 {
        crate::verr!("[VIDEO]   Registering the cover scheme handler failed");
    }
}

#[cfg(test)]
#[path = "../../tests/unit/ui/cover_scheme.rs"]
mod tests;
