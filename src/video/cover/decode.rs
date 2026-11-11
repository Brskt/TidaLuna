use crate::util::mp4_samples::{SampleBudget, sample_bytes};
use shiguredo_mp4::TrackKind;
use shiguredo_mp4::bitstream::h264::{LengthSize, length_prefixed_to_annexb};
use shiguredo_mp4::boxes::AvccBox;
use shiguredo_mp4::boxes::SampleEntry;
use shiguredo_mp4::demux::{Input, Mp4FileDemuxer};

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
    // Charging the declared size is enough: the Annex-B rewrite grows a sample at most fourfold,
    // and the parameter sets prepended to sync samples are bounded by the count.
    let mut budget = SampleBudget::new("cover", MAX_COVER_SAMPLES, file.len());

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
#[path = "../../../tests/unit/video/cover/decode.rs"]
mod tests;
