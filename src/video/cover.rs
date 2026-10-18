//! Decoding an animated album cover: a plain MP4 on `resources.tidal.com`, H.264, no audio.
//!
//! Distinct from the clip path in every way that matters: no manifest, no signed URL, no
//! seek, and the file is small enough to hold whole in memory.

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
/// must never reach a decoder.
fn is_cover_url(url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    parsed.scheme() == "https"
        && parsed.host_str() == Some("resources.tidal.com")
        && parsed.path().starts_with("/videos/")
}

/// The largest variant we ask for, in pixels on a side. At 1280 the 300 frames every variant
/// carries are 738 MiB at the decoder's strides, well past `MAX_DECODED_BYTES`, and a real
/// cover cannot finish; at 640 they are 193 MiB and fit. The canvas fills the same box either
/// way, and a quarter of the pixels is no visible loss on an animation.
const MAX_COVER_EDGE: u32 = 640;

/// The same cover at `MAX_COVER_EDGE`, or `None` unless the URL names a square `<n>x<n>.mp4`
/// above it. Only the last segment changes, which keeps it inside what `is_cover_url` admits.
fn capped_cover_url(url: &str) -> Option<String> {
    let mut parsed = url::Url::parse(url).ok()?;
    let last = parsed.path_segments()?.next_back()?.to_owned();
    let (width, height) = last.strip_suffix(".mp4")?.split_once('x')?;
    let width: u32 = width.parse().ok()?;
    let height: u32 = height.parse().ok()?;
    if width != height || width <= MAX_COVER_EDGE {
        return None;
    }
    {
        let mut segments = parsed.path_segments_mut().ok()?;
        segments.pop();
        segments.push(&format!("{MAX_COVER_EDGE}x{MAX_COVER_EDGE}.mp4"));
    }
    Some(parsed.to_string())
}

/// The URL a cover is actually fetched from, which is also the work's identity: capping before the
/// work starts lets two frames naming different variants share it. TIDAL writes a variant literal
/// at each call site (an album header asks for `1280x1280` whatever its box measures). Variants at
/// or under the cap are different files and stay apart.
pub(crate) fn canonical_cover_url(url: &str) -> String {
    match capped_cover_url(url) {
        Some(capped) => {
            crate::vprintln!("[VIDEO]   Cover capped to {MAX_COVER_EDGE}px on a side");
            capped
        }
        None => url.to_string(),
    }
}

/// How many requesters still want one cover: one frame giving up must not cancel what the
/// others wait for, and the question the work asks is a count reaching zero.
///
/// Joining is a compare-and-swap, not an increment: zero is a one-way door. Past it the work
/// may stop at its next boundary, and a latecomer that incremented would be handed an abandoned
/// answer; `try_join` refuses instead, and its caller starts a fresh job.
pub(crate) struct CoverInterest {
    waiters: std::sync::atomic::AtomicUsize,
}

impl CoverInterest {
    /// One requester: the one that is creating the job.
    pub(crate) fn new() -> Self {
        Self {
            waiters: std::sync::atomic::AtomicUsize::new(1),
        }
    }

    /// Add a requester to a job already running, unless it has none left.
    pub(crate) fn try_join(&self) -> bool {
        use std::sync::atomic::Ordering;
        let mut current = self.waiters.load(Ordering::Relaxed);
        loop {
            if current == 0 {
                return false;
            }
            match self.waiters.compare_exchange_weak(
                current,
                current + 1,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(seen) => current = seen,
            }
        }
    }

    /// One requester stops waiting. Saturating: a release that arrived twice cannot wrap the
    /// count back into interest and revive work nobody wants.
    pub(crate) fn release(&self) {
        use std::sync::atomic::Ordering;
        let _ = self
            .waiters
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                Some(n.saturating_sub(1))
            });
    }

    /// Nobody is waiting: everything from here on would be work for no reader.
    pub(crate) fn abandoned(&self) -> bool {
        self.waiters.load(std::sync::atomic::Ordering::Relaxed) == 0
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

/// Fetch a cover whole, or stop early and return `None` once nobody is waiting for it.
///
/// Small enough to hold in memory, and the demuxer wants it that way: it does no IO of its own and
/// is documented as accepting the entire file in one call.
///
/// `None` rather than an error, as in `decode_cover`.
async fn fetch_cover(url: &str, interest: &CoverInterest) -> anyhow::Result<Option<Vec<u8>>> {
    if !is_cover_url(url) {
        crate::vprintln!(
            "[VIDEO]   Refused a cover url outside the artwork host: {}",
            crate::util::refused_host(url)
        );
        anyhow::bail!("cover url is not a TIDAL artwork video url");
    }

    let resp = COVER_CLIENT
        .get(url)
        .timeout(COVER_TIMEOUT)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("{}", crate::util::network_error_text(e)))?;
    if !resp.status().is_success() {
        anyhow::bail!("artwork host answered {}", resp.status());
    }

    let mut out = Vec::new();
    let mut stream = resp.bytes_stream();
    {
        use futures_util::StreamExt;
        while let Some(chunk) = stream.next().await {
            // Asked once per chunk, a boundary where stopping is free; without it an abandoned
            // cover downloads to its last byte, or to `COVER_TIMEOUT`.
            if interest.abandoned() {
                return Ok(None);
            }
            let chunk =
                chunk.map_err(|e| anyhow::anyhow!("{}", crate::util::network_error_text(e)))?;
            if out.len() + chunk.len() > MAX_COVER_BYTES {
                anyhow::bail!("cover exceeds the ceiling of {MAX_COVER_BYTES} bytes");
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
/// The strides travel with the planes because the decoder is free to pad each row past `width`,
/// and a consumer addressing a padded plane by `width` alone reads the padding as picture. They
/// are what the transport writes into its header and what the shader uses to sample.
pub(crate) struct CoverFrame {
    pub(crate) width: usize,
    pub(crate) height: usize,
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

/// Decoded frames are held all at once, and a 1280x1280 frame is 2.58 MB at the decoder's
/// strides. This bounds the total rather than the count, because the count says nothing about
/// resolution.
const MAX_DECODED_BYTES: usize = 256 * 1024 * 1024;

/// Refuse a frame that is not the size of the ones before it.
///
/// The transport states one size per cover, in its header, and an `avc1` track may legally
/// carry a second sample entry whose SPS names new dimensions (`demux_cover_samples` lets it
/// through, the codec being unchanged). Refused rather than carried per frame: a frame that
/// disagrees with the header throws inside the page's animation callback, where nothing catches
/// it, and freezes the canvas over the hidden still artwork. Refused here, the still stays.
fn refuse_resolution_change(
    frames: &[CoverFrame],
    width: usize,
    height: usize,
) -> anyhow::Result<()> {
    let Some(first) = frames.first() else {
        return Ok(());
    };
    if (first.width, first.height) != (width, height) {
        anyhow::bail!(
            "cover changes resolution mid-stream, from {}x{} to {width}x{height}",
            first.width,
            first.height,
        );
    }
    Ok(())
}

/// Copy one decoded picture out and charge it against the ceiling.
fn push_decoded_frame(
    frames: &mut Vec<CoverFrame>,
    decoded_bytes: &mut usize,
    yuv: &openh264::decoder::DecodedYUV<'_>,
) -> anyhow::Result<()> {
    use openh264::formats::YUVSource;

    let (width, height) = yuv.dimensions();
    // Checked before the planes are copied: a frame that will be refused is not worth copying.
    refuse_resolution_change(frames, width, height)?;

    let (y_stride, u_stride, v_stride) = yuv.strides();
    let y = yuv.y().to_vec();
    let u = yuv.u().to_vec();
    let v = yuv.v().to_vec();
    *decoded_bytes = decoded_bytes
        .checked_add(y.len() + u.len() + v.len())
        .ok_or_else(|| anyhow::anyhow!("cover decoded size overflows"))?;
    if *decoded_bytes > MAX_DECODED_BYTES {
        anyhow::bail!("cover decoded frames exceed the ceiling of {MAX_DECODED_BYTES} bytes");
    }
    frames.push(CoverFrame {
        width,
        height,
        y,
        u,
        v,
        y_stride,
        u_stride,
        v_stride,
        // Filled once every picture is out, by `assign_presentation_times`. No frame ever leaves
        // `decode_cover` still carrying this: a count that cannot be paired refuses outright.
        pts_micros: 0,
    });
    Ok(())
}

/// Pair each decoded frame with its presentation time.
///
/// The decoder hands back no usable timestamp of its own (`DecodedYUV::timestamp` is built from
/// an input stamp this path never sets), so the times come from the samples. They cannot be
/// paired as they arrive: a stream that reorders emits pictures in display order while the
/// samples arrive in decode order, and covers reorder. Sorting the samples' presentation times
/// and walking them alongside the frames restores the correspondence. A count that does not
/// match is refused rather than truncated: a silent short pairing would mistime the whole
/// animation instead of failing.
fn assign_presentation_times(
    frames: &mut [CoverFrame],
    samples: &[CoverSample],
) -> anyhow::Result<()> {
    if frames.len() != samples.len() {
        anyhow::bail!(
            "cover decoded {} frames for {} samples, so their presentation times cannot be paired",
            frames.len(),
            samples.len()
        );
    }
    let mut times: Vec<i64> = samples.iter().map(|s| s.pts_micros).collect();
    times.sort_unstable();
    for (frame, pts) in frames.iter_mut().zip(times) {
        frame.pts_micros = pts;
    }
    Ok(())
}

/// Every frame, or `None` once nobody waits for the answer: being abandoned is not the file's
/// fault (see `CoverOutcome::Abandoned`).
fn decode_cover(
    file: &[u8],
    api: openh264::OpenH264API,
    interest: &CoverInterest,
) -> anyhow::Result<Option<Vec<CoverFrame>>> {
    use openh264::decoder::{Decoder, DecoderConfig, Flush};

    let samples = demux_cover_samples(file)?;
    if interest.abandoned() {
        return Ok(None);
    }

    // `Flush::NoFlush`, never the crate's default. `DecoderConfig::new()` sets `Flush::Flush`, and
    // `decode` passes `Flush::Auto`, which resolves to flushing after every single call. On a
    // stream that reorders, that flush takes a picture out of the buffer later frames still
    // reference, and OpenH264 answers `dsOutOfMemory` (upstream issue #50, error string and all).
    // The crate's own B-frame test and its MP4 example both run `NoFlush` plus a final drain, and
    // its `Flush` docs recommend the drain regardless.
    let config = DecoderConfig::new().flush_after_decode(Flush::NoFlush);
    let mut decoder = Decoder::with_api_config(api, config)
        .map_err(|e| anyhow::anyhow!("starting the cover decoder failed: {e}"))?;

    let mut frames = Vec::new();
    let mut decoded_bytes: usize = 0;
    for sample in &samples {
        // Read once per frame: this loop is the expensive part, and an atomic load costs nothing
        // against it.
        if interest.abandoned() {
            return Ok(None);
        }
        let decoded = decoder
            .decode(&sample.annex_b)
            .map_err(|e| anyhow::anyhow!("decoding a cover frame failed: {e}"))?;
        // `None` is ordinary: the decoder holds frames back until it has enough.
        if let Some(yuv) = decoded {
            push_decoded_frame(&mut frames, &mut decoded_bytes, &yuv)?;
        }
    }

    // The loop's own check precedes each decode, never this drain.
    if interest.abandoned() {
        return Ok(None);
    }
    // The pictures held back for reordering only come out here. Without this drain the tail of
    // every cover is missing, which is why the crate counts its own B-frame test after it.
    let remaining = decoder
        .flush_remaining()
        .map_err(|e| anyhow::anyhow!("draining the cover decoder failed: {e}"))?;
    for yuv in &remaining {
        push_decoded_frame(&mut frames, &mut decoded_bytes, yuv)?;
    }

    if frames.is_empty() {
        anyhow::bail!("cover decoded to no frames");
    }
    assign_presentation_times(&mut frames, &samples)?;
    Ok(Some(frames))
}

/// What a cover load produced. Four outcomes rather than a `Result`, because "no codec" is an
/// ordinary state with a name the user is owed, not a failure: the reason has to reach the UI
/// rather than a log line, and each `NoDecoder` wants different words in front of the user.
///
/// `T` is what the caller's `finish` made of the frames, never the frames themselves: see
/// [`load_cover`] for why that last step runs inside the load.
pub(crate) enum CoverOutcome<T> {
    Decoded(T),
    NoCodec(crate::video::codec::NoDecoder),
    Failed(String),
    /// The request stopped wanting its answer before the work finished. Its own outcome rather
    /// than a failure: nobody is owed a reason, and logging it as an error would report the
    /// page behaving correctly.
    Abandoned,
}

/// Fetch and decode one animated cover, provisioning the decoder on first need. The decode is
/// CPU-bound and runs on the blocking pool.
///
/// `interest` is read at every boundary where stopping is free: a cover nobody waits for should
/// not finish downloading, and above all not decoding (up to [`MAX_DECODED_BYTES`], one job per
/// track while skipping through a queue).
///
/// `finish` runs on the same blocking thread right after the decode, behind one more read of
/// `interest`: for the scheme it copies the whole cover, which on the async task would hold a
/// runtime worker.
///
/// `source` is already canonical ([`canonical_cover_url`]): the caller keys the job on it
/// before deciding whether to share it.
pub(crate) async fn load_cover<T: Send + 'static>(
    source: &str,
    interest: std::sync::Arc<CoverInterest>,
    finish: impl FnOnce(Vec<CoverFrame>) -> T + Send + 'static,
) -> CoverOutcome<T> {
    let path = match crate::video::codec::decoder().await {
        Ok(path) => path,
        Err(why) => return CoverOutcome::NoCodec(why),
    };
    if interest.abandoned() {
        return CoverOutcome::Abandoned;
    }

    let file = match fetch_cover(source, &interest).await {
        Ok(Some(file)) => file,
        Ok(None) => return CoverOutcome::Abandoned,
        Err(e) => return CoverOutcome::Failed(e.to_string()),
    };
    if interest.abandoned() {
        return CoverOutcome::Abandoned;
    }

    let decoded = tokio::task::spawn_blocking(move || -> anyhow::Result<Option<T>> {
        let api = match crate::video::codec::load_api(&path) {
            Ok(api) => api,
            Err(e) => {
                // The supervisor proved this file loads: it changed since.
                crate::video::codec::recheck();
                return Err(e);
            }
        };
        let Some(frames) = decode_cover(&file, api, &interest)? else {
            return Ok(None);
        };
        if interest.abandoned() {
            return Ok(None);
        }
        Ok(Some(finish(frames)))
    })
    .await;

    match decoded {
        Ok(Ok(Some(finished))) => CoverOutcome::Decoded(finished),
        Ok(Ok(None)) => CoverOutcome::Abandoned,
        Ok(Err(e)) => CoverOutcome::Failed(e.to_string()),
        Err(e) => CoverOutcome::Failed(format!("the cover decode task failed: {e}")),
    }
}

#[cfg(test)]
#[path = "../../tests/unit/video/cover.rs"]
mod tests;
