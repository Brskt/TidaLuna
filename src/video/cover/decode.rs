use std::collections::HashSet;
use std::sync::Arc;

use crate::util::mp4_samples::{SampleBudget, sample_bytes};
use shiguredo_mp4::TrackKind;
use shiguredo_mp4::bitstream::h264::LengthSize;
use shiguredo_mp4::boxes::AvccBox;
use shiguredo_mp4::boxes::SampleEntry;
use shiguredo_mp4::demux::{Input, Mp4FileDemuxer};

use super::nal::{CallWork, admit_nal};
use crate::video::codec::FailureClass;

/// A 4-byte start code. Three is also legal Annex-B, but four is what the crate emits and
/// what every muxer writes at an access-unit boundary.
const START_CODE: [u8; 4] = [0, 0, 0, 1];

/// What the `avcC` box carries that the decoder needs before its first slice.
pub(crate) struct ParameterSets {
    sps: Vec<Vec<u8>>,
    pps: Vec<Vec<u8>>,
    length_size: LengthSize,
}

/// Every SPS and PPS of one `avcC` together. Real ones are small; the box itself may declare
/// 18 MB, all of it input.
const MAX_PARAMETER_SET_BYTES: usize = 16 * 1024;

/// `length_size_minus_one` is two bits, and the value 2 is reserved by ISO/IEC 14496-15.
/// `LengthSize` has three variants and cannot hold it, so the refusal is structural rather
/// than a check we could forget: an illegal width never becomes a value.
pub(crate) fn parameter_sets(avcc: &AvccBox) -> anyhow::Result<ParameterSets> {
    let length_size = LengthSize::from_length_size_minus_one(avcc.length_size_minus_one.get())
        .map_err(|e| anyhow::anyhow!("avcC declares a reserved length prefix: {e}"))?;
    let total = avcc
        .sps_list
        .iter()
        .chain(&avcc.pps_list)
        .fold(0usize, |total, nal| total.saturating_add(nal.len()));
    if total > MAX_PARAMETER_SET_BYTES {
        anyhow::bail!(
            "avcC carries {total} bytes of SPS and PPS, past the cap of {MAX_PARAMETER_SET_BYTES}"
        );
    }
    // Both lists: each entry reaches the decoder behind a start code, which reads its header
    // byte rather than the list it came from.
    for nal in avcc.sps_list.iter().chain(&avcc.pps_list) {
        admit_nal(nal)?;
    }
    Ok(ParameterSets {
        sps: avcc.sps_list.clone(),
        pps: avcc.pps_list.clone(),
        length_size,
    })
}

impl ParameterSets {
    /// What a fresh decoder needs before its first slice: every SPS then every PPS, each behind a
    /// start code. Built once per sample entry, then shared by every unit of that entry.
    pub(crate) fn prefix(&self) -> anyhow::Result<Vec<u8>> {
        if self.sps.is_empty() || self.pps.is_empty() {
            anyhow::bail!("avcC carries no SPS or no PPS; the stream cannot be started");
        }
        let mut out = Vec::new();
        for nal in self.sps.iter().chain(self.pps.iter()) {
            out.extend_from_slice(&START_CODE);
            out.extend_from_slice(nal);
        }
        Ok(out)
    }
}

/// One shared copy per distinct value: equal sample entries share their prefix or config; what
/// the units hold is bounded by the distinct entries, not by the samples.
pub(crate) fn intern(seen: &mut HashSet<Arc<[u8]>>, bytes: Vec<u8>) -> Arc<[u8]> {
    if let Some(shared) = seen.get(bytes.as_slice()) {
        return shared.clone();
    }
    let shared: Arc<[u8]> = bytes.into();
    seen.insert(shared.clone());
    shared
}

/// Rewrite one length-prefixed sample into Annex-B over `out`, and say whether it holds an IDR
/// slice: only an IDR starts a decoder, whatever the container's sync flag claims. Written as it
/// is read, with no NAL list first. Its slices are charged against one `decode()` call's budget.
pub(crate) fn annex_b_access_unit(
    sample: &[u8],
    params: &ParameterSets,
    out: &mut Vec<u8>,
) -> anyhow::Result<bool> {
    out.clear();
    let width = params.length_size.bytes();
    let mut rest = sample;
    let mut idr = false;
    let mut work = CallWork::default();
    while !rest.is_empty() {
        let (field, tail) = rest
            .split_at_checked(width)
            .ok_or_else(|| anyhow::anyhow!("a NAL length field runs past the sample"))?;
        let len = field
            .iter()
            .fold(0usize, |len, byte| len << 8 | usize::from(*byte));
        if len == 0 {
            anyhow::bail!("a NAL declares a length of 0");
        }
        let (nal, tail) = tail
            .split_at_checked(len)
            .ok_or_else(|| anyhow::anyhow!("a NAL of {len} bytes runs past the sample"))?;
        let header = nal[0];
        if header & 0x80 != 0 {
            anyhow::bail!("a NAL sets its forbidden_zero_bit");
        }
        admit_nal(nal)?;
        work.charge(nal)?;
        idr |= header & 0x1F == IDR_SLICE;
        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(nal);
        rest = tail;
    }
    Ok(idr)
}

/// `nal_unit_type` of a coded slice of an IDR picture (ITU-T H.264 table 7-1).
const IDR_SLICE: u8 = 5;

/// One decode call's bytes over `out`: the entry's parameter sets in front of an IDR, then the
/// sample. A fresh decoder has none, and one that holds them drops an identical copy.
pub(crate) fn access_unit(prefix: &[u8], idr: bool, sample: &[u8], out: &mut Vec<u8>) {
    out.clear();
    if idr {
        out.extend_from_slice(prefix);
    }
    out.extend_from_slice(sample);
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

/// One sample, lifted out of the demuxer's borrow: its Annex-B slices, and its entry's parameter
/// sets shared with every other sample of that entry, put in front at decode time on an IDR.
struct CoverSample {
    annex_b: Vec<u8>,
    idr: bool,
    prefix: Arc<[u8]>,
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

    let mut params: Option<(ParameterSets, Arc<[u8]>)> = None;
    let mut prefixes = HashSet::new();
    let mut samples = Vec::new();
    // Charging the declared size bounds the samples: Annex-B adds 3 bytes per NAL at most. The
    // parameter sets are held once per distinct entry, each under `MAX_PARAMETER_SET_BYTES`.
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
                let sets = parameter_sets(&avc1.avcc_box)?;
                let prefix = intern(&mut prefixes, sets.prefix()?);
                params = Some((sets, prefix));
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
        let Some((params, prefix)) = params.as_ref() else {
            anyhow::bail!("cover sample arrived before its avcC");
        };

        // Charged first, so a file whose tables point every sample at one region is refused
        // before that region is converted and copied even once more.
        budget.charge(sample.data_size)?;

        let bytes = sample_bytes(file, sample.data_offset, sample.data_size)
            .ok_or_else(|| anyhow::anyhow!("cover sample lies outside the file"))?;
        let mut annex_b = Vec::new();
        let idr = annex_b_access_unit(bytes, params, &mut annex_b)?;

        // `timestamp` is the decode timestamp; presentation is it plus the composition
        // offset, which is absent on a stream with no B-frames.
        let pts = presentation_time(sample.timestamp, sample.composition_time_offset)?;
        let pts_micros = pts.checked_mul(1_000_000).ok_or_else(|| {
            anyhow::anyhow!("cover sample presentation time overflows converting to microseconds")
        })? / timescale;
        samples.push(CoverSample {
            annex_b,
            idr,
            prefix: prefix.clone(),
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
    /// The access unit being decoded, refilled for each sample.
    scratch: Vec<u8>,
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
        let samples = demux_cover_samples(file).map_err(OpenError::Cover)?;
        let frame_count = samples.len();
        let times = PresentationTimes::new(samples.iter().map(|s| s.pts_micros).collect());

        let decoder = crate::video::codec::start_decoder(api)
            .map_err(|e| OpenError::Decoder(e.to_string()))?;

        let mut stream = Self {
            samples: samples.into_iter(),
            scratch: Vec::new(),
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
            access_unit(
                &sample.prefix,
                sample.idr,
                &sample.annex_b,
                &mut self.scratch,
            );
            let decoded = self
                .decoder
                .decode(&self.scratch)
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
