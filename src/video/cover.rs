//! Decoding an animated album cover: a plain MP4 on `resources.tidal.com`, H.264, no audio.
//!
//! Distinct from the clip path in every way that matters: no manifest, no signed URL, no
//! seek, and the file is small enough to hold whole in memory. The decoded frames are never held
//! whole: [`DecodeStream`] hands them out one at a time to the page, which transcodes them once.

/// The bytes of one sample, or `None` when the file's tables do not describe a range this
/// buffer actually contains.
///
/// `data_offset` and `data_size` are read out of `stco`/`co64`/`stsz`, which is untrusted
/// input. `shiguredo_mp4` validates them against each other and against integer overflow,
/// but never against the length of the buffer the caller holds: its own documentation
/// example slices directly on them and panics on a truncated file. This is that missing
/// check, and it is the reason every sample read goes through here.
fn sample_bytes(file: &[u8], offset: u64, size: usize) -> Option<&[u8]> {
    if size == 0 {
        return None;
    }
    let start = usize::try_from(offset).ok()?;
    let end = start.checked_add(size)?;
    file.get(start..end)
}

use shiguredo_mp4::bitstream::h264::{LengthSize, length_prefixed_to_annexb};
use shiguredo_mp4::boxes::AvccBox;

use crate::video::codec::FailureClass;

/// A 4-byte start code. Three is also legal Annex-B, but four is what the crate emits and
/// what every muxer writes at an access-unit boundary.
const START_CODE: [u8; 4] = [0, 0, 0, 1];

/// What the `avcC` box carries that the decoder needs before its first slice.
struct ParameterSets {
    sps: Vec<Vec<u8>>,
    pps: Vec<Vec<u8>>,
    length_size: LengthSize,
}

/// `length_size_minus_one` is two bits, and the value 2 is reserved by ISO/IEC 14496-15.
/// `LengthSize` has three variants and cannot hold it, so the refusal is structural rather
/// than a check we could forget: an illegal width never becomes a value.
fn parameter_sets(avcc: &AvccBox) -> anyhow::Result<ParameterSets> {
    let length_size = LengthSize::from_length_size_minus_one(avcc.length_size_minus_one.get())
        .map_err(|e| anyhow::anyhow!("cover avcC declares a reserved length prefix: {e}"))?;
    Ok(ParameterSets {
        sps: avcc.sps_list.clone(),
        pps: avcc.pps_list.clone(),
        length_size,
    })
}

/// Rewrite one sample into an Annex-B access unit, prefixing the parameter sets on a
/// keyframe.
///
/// The parameter sets go in front of every keyframe rather than once at the start: a cover
/// loops, and a decoder restarted mid-loop would otherwise get slices it cannot interpret.
/// They are deliberately not repeated on non-keyframes, where the decoder already holds them.
fn annex_b_access_unit(
    sample: &[u8],
    params: &ParameterSets,
    keyframe: bool,
    out: &mut Vec<u8>,
) -> anyhow::Result<()> {
    out.clear();
    if keyframe {
        if params.sps.is_empty() || params.pps.is_empty() {
            anyhow::bail!("cover avcC carries no SPS or no PPS; the stream cannot be started");
        }
        for nal in params.sps.iter().chain(params.pps.iter()) {
            out.extend_from_slice(&START_CODE);
            out.extend_from_slice(nal);
        }
    }
    let converted = length_prefixed_to_annexb(sample, params.length_size)
        .map_err(|e| anyhow::anyhow!("cover sample is not valid length-prefixed H.264: {e}"))?;
    out.extend_from_slice(&converted);
    Ok(())
}

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

use shiguredo_mp4::TrackKind;
use shiguredo_mp4::boxes::SampleEntry;
use shiguredo_mp4::demux::{Input, Mp4FileDemuxer};

/// One decoded frame, owned. The decoder's own `DecodedYUV` borrows the decoder and cannot
/// outlive the next `decode` call; the planes are copied out here.
///
/// The strides travel with the planes because the decoder is free to pad each row past the width,
/// and a consumer addressing a padded plane by that width alone reads the padding as picture. They
/// are what each frame's record states and what the page's `VideoFrame` layout is built from.
pub(crate) struct CoverFrame {
    pub(crate) y: Vec<u8>,
    pub(crate) u: Vec<u8>,
    pub(crate) v: Vec<u8>,
    pub(crate) y_stride: usize,
    pub(crate) u_stride: usize,
    pub(crate) v_stride: usize,
    pub(crate) pts_micros: i64,
}

/// One sample, lifted out of the demuxer's borrow.
struct CoverSample {
    annex_b: Vec<u8>,
    pts_micros: i64,
}

/// Presentation time from an untrusted `stts` timestamp and its composition offset. Refused
/// rather than wrapped: a wrapped time looks plausible and is wrong.
fn presentation_time(timestamp: u64, offset: Option<i64>) -> anyhow::Result<i64> {
    let dts = i64::try_from(timestamp).map_err(|_| {
        anyhow::anyhow!("cover sample timestamp does not fit a signed 64-bit value")
    })?;
    dts.checked_add(offset.unwrap_or(0))
        .ok_or_else(|| anyhow::anyhow!("cover sample presentation time overflows"))
}

/// A cover is a short loop: a generous 60 seconds at 30 fps is 1800 samples. The ceiling is
/// not a guess at content, it is a bound on what the file may *declare*: `stts` is run-length
/// encoded and `stsz` in fixed mode stores only a count, so a few bytes can ask for billions
/// of samples. Refuse rather than truncate, as `player::dash`'s own segment ceiling does.
const MAX_COVER_SAMPLES: usize = 10_000;

/// What one track's samples may claim, charged as each one arrives. Two ceilings, neither
/// implying the other:
///
/// * the COUNT bounds what the tables may declare (see [`MAX_COVER_SAMPLES`]);
/// * the BYTE TOTAL bounds what the samples may point at. The demuxer checks `stco`/`co64`
///   offsets for overflow only, never for overlap: every chunk may name one region, each sample
///   kept as its own copy, and neither the count nor the download cap stops that aliasing.
///
/// The byte ceiling is the file's length: a well-formed track's samples occupy disjoint regions
/// of it, and aliasing is what exceeds it. Charging the declared size is enough: the Annex-B
/// rewrite grows a sample at most fourfold, and the parameter sets prepended to sync samples
/// are bounded by the count.
struct SampleBudget {
    file_len: usize,
    claimed: usize,
    count: usize,
}

impl SampleBudget {
    fn new(file_len: usize) -> Self {
        Self {
            file_len,
            claimed: 0,
            count: 0,
        }
    }

    /// Charged per sample, before anything is copied: a total computed up front would be the
    /// hostile value itself.
    fn charge(&mut self, data_size: usize) -> anyhow::Result<()> {
        if self.count >= MAX_COVER_SAMPLES {
            anyhow::bail!("cover exceeds the ceiling of {MAX_COVER_SAMPLES} samples");
        }
        let claimed = self.claimed.saturating_add(data_size);
        if claimed > self.file_len {
            anyhow::bail!(
                "cover samples claim {claimed} bytes of a {} byte file; refusing it",
                self.file_len
            );
        }
        self.claimed = claimed;
        self.count += 1;
        Ok(())
    }
}

/// Demux the file into Annex-B access units, in decode order.
///
/// Everything the demuxer hands back borrows it: each sample is copied out before the
/// next call. The `avcC` arrives on the first sample and then only when it changes, which is
/// why the parameter sets are captured the first time and reused.
fn demux_cover_samples(file: &[u8]) -> anyhow::Result<Vec<CoverSample>> {
    let mut demuxer = Mp4FileDemuxer::new();
    demuxer.handle_input(Input {
        position: 0,
        data: file,
    });

    let tracks = demuxer
        .tracks()
        .map_err(|e| anyhow::anyhow!("cover is not a readable MP4: {e}"))?;
    let video = tracks
        .iter()
        .find(|t| matches!(t.kind, TrackKind::Video))
        .ok_or_else(|| anyhow::anyhow!("cover carries no video track"))?;
    let track_id = video.track_id;
    let timescale = video.timescale.get() as i64;

    let mut params: Option<ParameterSets> = None;
    let mut samples = Vec::new();
    let mut scratch = Vec::new();
    let mut budget = SampleBudget::new(file.len());

    loop {
        let sample = match demuxer
            .next_sample()
            .map_err(|e| anyhow::anyhow!("reading a cover sample failed: {e}"))?
        {
            Some(s) => s,
            None => break,
        };
        if sample.track.track_id != track_id {
            continue;
        }
        match sample.sample_entry {
            Some(SampleEntry::Avc1(avc1)) => {
                params = Some(parameter_sets(&avc1.avcc_box)?);
            }
            // A later `stsd` index switched the track to a codec this path does not
            // decode. Leaving `params` as-is would feed non-AVC bytes to the H.264
            // decoder under stale SPS/PPS; bail instead.
            Some(_) if params.is_some() => {
                anyhow::bail!("cover's video track changed to a codec this path does not decode");
            }
            Some(_) => {
                anyhow::bail!("cover's video track opens on a codec this path does not decode");
            }
            None => {}
        }
        let Some(params) = params.as_ref() else {
            anyhow::bail!("cover sample arrived before its avcC");
        };

        // Charged first, so a file whose tables point every sample at one region is refused
        // before that region is converted and copied even once more.
        budget.charge(sample.data_size)?;

        let bytes = sample_bytes(file, sample.data_offset, sample.data_size)
            .ok_or_else(|| anyhow::anyhow!("cover sample lies outside the file"))?;
        annex_b_access_unit(bytes, params, sample.keyframe, &mut scratch)?;

        // `timestamp` is the decode timestamp; presentation is it plus the composition
        // offset, which is absent on a stream with no B-frames.
        let pts = presentation_time(sample.timestamp, sample.composition_time_offset)?;
        let pts_micros = pts.checked_mul(1_000_000).ok_or_else(|| {
            anyhow::anyhow!("cover sample presentation time overflows converting to microseconds")
        })? / timescale;
        samples.push(CoverSample {
            annex_b: scratch.clone(),
            pts_micros,
        });
    }

    if samples.is_empty() {
        anyhow::bail!("cover video track carries no samples");
    }
    Ok(samples)
}

/// The stream's header states one size per cover, yet an `avc1` track may carry a second sample
/// entry whose SPS names new dimensions (`demux_cover_samples` passes it: same codec). Refused,
/// not carried per frame: the page's encoder is configured once, for the header's size.
fn refuse_resolution_change(
    first: Option<(usize, usize)>,
    width: usize,
    height: usize,
) -> anyhow::Result<()> {
    match first {
        Some((w, h)) if (w, h) != (width, height) => {
            anyhow::bail!("cover changes resolution mid-stream, from {w}x{h} to {width}x{height}")
        }
        _ => Ok(()),
    }
}

/// Times for the decoded frames: the decoder returns none usable, and covers reorder; the samples'
/// times, sorted, are one FIFO the frames draw from. Rebased to zero: the edit list that would say
/// so is not read, and a loop starting late holds its last frame at every seam.
struct PresentationTimes {
    queue: std::collections::VecDeque<i64>,
    span: i64,
}

impl PresentationTimes {
    fn new(mut times: Vec<i64>) -> Self {
        times.sort_unstable();
        let first = times.first().copied().unwrap_or(0);
        let queue: std::collections::VecDeque<i64> = times.into_iter().map(|t| t - first).collect();
        let span = queue.back().copied().unwrap_or(0);
        Self { queue, span }
    }

    /// The next frame's time. A frame with no time left is refused rather than given one: a
    /// count that cannot be paired would mistime the whole animation instead of failing.
    fn next(&mut self) -> anyhow::Result<i64> {
        self.queue
            .pop_front()
            .ok_or_else(|| anyhow::anyhow!("cover decoded more frames than it has samples"))
    }
}

fn take_frame(
    yuv: &openh264::decoder::DecodedYUV<'_>,
    first: &mut Option<(usize, usize)>,
    times: &mut PresentationTimes,
) -> anyhow::Result<CoverFrame> {
    use openh264::formats::YUVSource;

    let (width, height) = yuv.dimensions();
    // Checked before the planes are copied: a frame that will be refused is not worth copying.
    refuse_resolution_change(*first, width, height)?;
    first.get_or_insert((width, height));

    let (y_stride, u_stride, v_stride) = yuv.strides();
    Ok(CoverFrame {
        y: yuv.y().to_vec(),
        u: yuv.u().to_vec(),
        v: yuv.v().to_vec(),
        y_stride,
        u_stride,
        v_stride,
        pts_micros: times.next()?,
    })
}

/// One cover decoded a frame at a time, in display order, from a file already downloaded.
///
/// Holds only the frames the decoder releases at once: a whole clip at 1280 runs to hundreds of
/// MiB decoded, and the page needs each frame only while it encodes it. No thread of its own: it
/// moves to whichever pool thread CEF reads on, and `next_frame` is all of its work.
pub(crate) struct DecodeStream {
    samples: std::vec::IntoIter<CoverSample>,
    decoder: openh264::decoder::Decoder,
    times: PresentationTimes,
    ready: std::collections::VecDeque<CoverFrame>,
    drained: bool,
    size: Option<(usize, usize)>,
    frame_count: usize,
}

/// Why a decode could not open: ours, or the cover's.
#[derive(Debug)]
pub(crate) enum OpenError {
    /// The decoder would not start; the cover says nothing.
    Decoder(String),
    /// The file would not demux, or decode to a first picture.
    Cover(anyhow::Error),
}

impl OpenError {
    pub(crate) fn class(&self) -> FailureClass {
        match self {
            Self::Decoder(_) => FailureClass::Unloadable,
            Self::Cover(_) => FailureClass::Corrupt,
        }
    }
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Decoder(e) => write!(f, "starting the cover decoder failed: {e}"),
            Self::Cover(e) => write!(f, "{e:#}"),
        }
    }
}

impl DecodeStream {
    /// Demux `file`, start the decoder, and decode up to the first picture: its size is what the
    /// stream's header states, and a file that never yields one is refused before any header.
    pub(crate) fn open(file: &[u8], api: openh264::OpenH264API) -> Result<Self, OpenError> {
        use openh264::decoder::{Decoder, DecoderConfig, Flush};

        let samples = demux_cover_samples(file).map_err(OpenError::Cover)?;
        let frame_count = samples.len();
        let times = PresentationTimes::new(samples.iter().map(|s| s.pts_micros).collect());

        // `Flush::NoFlush` and one final drain: `DecoderConfig::new()` with `decode`'s `Flush::Auto`
        // flushes after every call, which on a reordering stream evicts a referenced picture and
        // fails with `dsOutOfMemory` (openh264-rs#50). The crate's B-frame test and MP4 example agree.
        let config = DecoderConfig::new().flush_after_decode(Flush::NoFlush);
        let decoder =
            Decoder::with_api_config(api, config).map_err(|e| OpenError::Decoder(e.to_string()))?;

        let mut stream = Self {
            samples: samples.into_iter(),
            decoder,
            times,
            ready: std::collections::VecDeque::new(),
            drained: false,
            size: None,
            frame_count,
        };
        while stream.ready.is_empty() && stream.decode_more().map_err(OpenError::Cover)? {}
        if stream.ready.is_empty() {
            return Err(OpenError::Cover(anyhow::anyhow!(
                "cover decoded to no frames"
            )));
        }
        Ok(stream)
    }

    /// The size every frame has, in pixels.
    pub(crate) fn size(&self) -> (usize, usize) {
        self.size.unwrap_or((0, 0))
    }

    /// How many frames the stream yields: one per sample, or it fails.
    pub(crate) fn frame_count(&self) -> usize {
        self.frame_count
    }

    /// The last frame's presentation time, the first being zero.
    pub(crate) fn span_micros(&self) -> i64 {
        self.times.span
    }

    /// The next frame in display order, `None` once every sample's picture is out.
    pub(crate) fn next_frame(&mut self) -> anyhow::Result<Option<CoverFrame>> {
        while self.ready.is_empty() {
            if !self.decode_more()? {
                if !self.times.queue.is_empty() {
                    anyhow::bail!(
                        "cover decoded {} frames for {} samples",
                        self.frame_count - self.times.queue.len(),
                        self.frame_count
                    );
                }
                return Ok(None);
            }
        }
        Ok(self.ready.pop_front())
    }

    /// Feed the decoder one sample, or drain it once the samples are spent. False when there is
    /// nothing left to feed or drain.
    fn decode_more(&mut self) -> anyhow::Result<bool> {
        if let Some(sample) = self.samples.next() {
            let decoded = self
                .decoder
                .decode(&sample.annex_b)
                .map_err(|e| anyhow::anyhow!("decoding a cover frame failed: {e}"))?;
            // `None` is ordinary: the decoder holds frames back until it has enough.
            if let Some(yuv) = decoded {
                let frame = take_frame(&yuv, &mut self.size, &mut self.times)?;
                self.ready.push_back(frame);
            }
            return Ok(true);
        }
        if self.drained {
            return Ok(false);
        }
        self.drained = true;
        // The pictures held back for reordering only come out here. Without this drain the tail of
        // every cover is missing, which is why the crate counts its own B-frame test after it.
        let remaining = self
            .decoder
            .flush_remaining()
            .map_err(|e| anyhow::anyhow!("draining the cover decoder failed: {e}"))?;
        for yuv in &remaining {
            let frame = take_frame(yuv, &mut self.size, &mut self.times)?;
            self.ready.push_back(frame);
        }
        Ok(true)
    }
}

#[cfg(test)]
#[path = "../../tests/unit/video/cover.rs"]
mod tests;
