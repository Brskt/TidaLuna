//! One append split into the units `decode` feeds its decoders, every count and offset bounded.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::{anyhow, bail};
use shiguredo_mp4::Decode;
use shiguredo_mp4::boxes::{MoovBox, Mp4aBox, SampleEntry};
use shiguredo_mp4::demux::Fmp4SegmentDemuxer;
use shiguredo_mp4::descriptors::DecoderConfigDescriptor;

use crate::util::mp4_samples::{SampleBudget, sample_bytes};
use crate::video::cover::{ParameterSets, annex_b_access_unit, intern, parameter_sets};

use super::{MAX_FRAGMENT_SAMPLES, MAX_INIT_BYTES, MAX_MOOF_BYTES};

/// Cap on an AudioSpecificConfig, which every audio unit of an entry shares. Plain AAC-LC is
/// 2 bytes.
const MAX_ASC_BYTES: usize = 64;

/// What the page posts: the SourceBuffer's last init, the fragment appended before this one when
/// the two are contiguous (empty otherwise), and the appended bytes.
pub(crate) struct FragmentRequest<'a> {
    pub(crate) init: &'a [u8],
    pub(crate) preroll: &'a [u8],
    pub(crate) media: &'a [u8],
}

/// `u32 BE init_len | init | u32 BE preroll_len | preroll | media`.
pub(crate) fn parse_body(body: &[u8]) -> anyhow::Result<FragmentRequest<'_>> {
    let (init, rest) = length_prefixed(body, "init")?;
    let (preroll, media) = length_prefixed(rest, "preroll")?;
    if media.is_empty() {
        bail!("the fragment request carries no media");
    }
    Ok(FragmentRequest {
        init,
        preroll,
        media,
    })
}

/// A big-endian `u32` length and the bytes it counts, then the rest.
fn length_prefixed<'a>(data: &'a [u8], what: &str) -> anyhow::Result<(&'a [u8], &'a [u8])> {
    let (len, rest) = data
        .split_first_chunk::<4>()
        .ok_or_else(|| anyhow!("the fragment request ends before its {what} length"))?;
    let len = usize::try_from(u32::from_be_bytes(*len))
        .map_err(|_| anyhow!("the fragment request's {what} length does not fit memory"))?;
    if len > rest.len() {
        bail!("the fragment request's {what} runs past its body");
    }
    Ok(rest.split_at(len))
}

/// One top-level box: its type and the bytes it spans, header included.
#[derive(Clone, Copy, Debug)]
pub(crate) struct BoxSpan {
    pub(crate) kind: [u8; 4],
    pub(crate) start: usize,
    pub(crate) end: usize,
}

/// Walks `data` box by box, refusing one that runs past the buffer: the demuxer takes one
/// `moof` + `mdat` per call, and an append may hold several, or an init before them. Lazy, since
/// a body of empty 8-byte boxes would otherwise cost a span each; the first error ends the walk.
pub(crate) fn top_level_boxes(data: &[u8]) -> impl Iterator<Item = anyhow::Result<BoxSpan>> + '_ {
    TopLevelBoxes { data, start: 0 }
}

struct TopLevelBoxes<'a> {
    data: &'a [u8],
    start: usize,
}

impl Iterator for TopLevelBoxes<'_> {
    type Item = anyhow::Result<BoxSpan>;

    fn next(&mut self) -> Option<Self::Item> {
        let rest = self.data.get(self.start..).filter(|r| !r.is_empty())?;
        let span = self.span(rest);
        // Retried at the same offset, a refused box would be refused forever.
        self.start = span.as_ref().map_or(self.data.len(), |span| span.end);
        Some(span)
    }
}

impl TopLevelBoxes<'_> {
    fn span(&self, rest: &[u8]) -> anyhow::Result<BoxSpan> {
        let (size, kind, header_len) = box_header(rest)?;
        let size = match size {
            // Size 0: the box runs to the end of the buffer.
            0 => rest.len(),
            n => usize::try_from(n).map_err(|_| anyhow!("a box's size does not fit memory"))?,
        };
        if size < header_len {
            bail!(
                "a {} box declares less than its header",
                kind.escape_ascii()
            );
        }
        let end = self
            .start
            .checked_add(size)
            .filter(|&end| end <= self.data.len())
            .ok_or_else(|| anyhow!("a {} box runs past the buffer", kind.escape_ascii()))?;
        Ok(BoxSpan {
            kind,
            start: self.start,
            end,
        })
    }
}

/// A box's declared size (0 meaning "to the end"), its type and its header's length.
fn box_header(data: &[u8]) -> anyhow::Result<(u64, [u8; 4], usize)> {
    let (head, rest) = data
        .split_first_chunk::<8>()
        .ok_or_else(|| anyhow!("a box header is cut short"))?;
    let [s0, s1, s2, s3, k0, k1, k2, k3] = *head;
    let kind = [k0, k1, k2, k3];
    match u32::from_be_bytes([s0, s1, s2, s3]) {
        1 => {
            let large = rest.first_chunk::<8>().ok_or_else(|| {
                anyhow!("a {} box's 64-bit size is cut short", kind.escape_ascii())
            })?;
            Ok((u64::from_be_bytes(*large), kind, 16))
        }
        n => Ok((u64::from(n), kind, 8)),
    }
}

fn box_body<'a>(data: &'a [u8], span: &BoxSpan) -> anyhow::Result<&'a [u8]> {
    let whole = data
        .get(span.start..span.end)
        .ok_or_else(|| anyhow!("a box lies outside its buffer"))?;
    let (_, _, header_len) = box_header(whole)?;
    whole
        .get(header_len..)
        .ok_or_else(|| anyhow!("a box is shorter than its header"))
}

/// Each track's edit offset, as Chromium applies it: the first `elst` entry's media time, an
/// empty edit (negative) counting as none. The demuxer does not apply edit lists at all.
pub(crate) fn edit_offsets(init: &[u8]) -> anyhow::Result<HashMap<u32, i64>> {
    let mut boxes = top_level_boxes(init);
    let span = loop {
        let span = boxes
            .next()
            .ok_or_else(|| anyhow!("the init segment has no moov"))??;
        if &span.kind == b"moov" {
            break span;
        }
    };
    let bytes = init
        .get(span.start..span.end)
        .ok_or_else(|| anyhow!("the init's moov lies outside it"))?;
    let (moov, _) =
        MoovBox::decode(bytes).map_err(|e| anyhow!("the init's moov does not decode: {e}"))?;
    Ok(moov
        .trak_boxes
        .iter()
        .filter_map(|trak| {
            let first = trak.edts_box.as_ref()?.elst_box.as_ref()?.entries.first()?;
            (first.media_time >= 0).then_some((trak.tkhd_box.track_id, first.media_time))
        })
        .collect())
}

/// AAC-LC in 1024-sample frames on one or two channels, from its AudioSpecificConfig. Refused
/// before decoding: symphonia rejects SBR, but SBR found mid-stream decodes at half rate silently.
pub(crate) fn aac_lc(asc: &[u8]) -> anyhow::Result<()> {
    let mut bits = Bits::new(asc);
    let object_type = bits.read(5)?;
    if object_type != 2 {
        bail!("the audio is not AAC-LC (object type {object_type})");
    }
    let frequency = bits.read(4)?;
    if frequency > 12 {
        bail!("the audio's sampling frequency index {frequency} is not a table rate");
    }
    let channels = bits.read(4)?;
    if !(1..=2).contains(&channels) {
        bail!("the audio's channel configuration {channels} is not mono or stereo");
    }
    if bits.read(1)? != 0 {
        bail!("the audio uses 960-sample frames");
    }
    if bits.read(1)? != 0 || bits.read(1)? != 0 {
        bail!("the audio's AAC-LC config carries a core coder or an extension");
    }
    // Explicit backward-compatible signalling: sync extension 0x2B7, then SBR's object type and
    // its present flag. Absent, or present with the flag clear, the stream is plain LC.
    if bits.left() >= 16 && bits.read(11)? == 0x2B7 && bits.read(5)? == 5 && bits.read(1)? == 1 {
        bail!("the audio carries SBR (HE-AAC)");
    }
    Ok(())
}

/// MSB-first reads over an AudioSpecificConfig.
struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn left(&self) -> usize {
        (self.data.len() * 8).saturating_sub(self.pos)
    }

    fn read(&mut self, n: u32) -> anyhow::Result<u32> {
        let mut value = 0u32;
        for _ in 0..n {
            let byte = self
                .data
                .get(self.pos / 8)
                .ok_or_else(|| anyhow!("the audio's AudioSpecificConfig is cut short"))?;
            value = (value << 1) | u32::from((byte >> (7 - self.pos % 8)) & 1);
            self.pos += 1;
        }
        Ok(value)
    }
}

/// One H.264 sample in Annex-B, and its entry's parameter sets, shared, which go in front of it
/// at decode time when it holds an IDR slice.
pub(crate) struct VideoUnit {
    pub(crate) annex_b: Vec<u8>,
    pub(crate) idr: bool,
    pub(crate) prefix: Arc<[u8]>,
    pub(crate) pts: i64,
    pub(crate) duration: u32,
    pub(crate) timescale: u32,
}

/// One raw AAC frame and the AudioSpecificConfig it decodes with, shared by its entry's frames.
pub(crate) struct AudioUnit {
    pub(crate) data: Vec<u8>,
    pub(crate) pts: i64,
    pub(crate) duration: u32,
    pub(crate) timescale: u32,
    pub(crate) asc: Arc<[u8]>,
}

/// An append's samples in decode order, and the preroll's last AAC frame when it can prime the
/// decoder for the first one.
pub(crate) struct DemuxedFragment {
    pub(crate) video: Vec<VideoUnit>,
    pub(crate) audio: Vec<AudioUnit>,
    pub(crate) preroll_audio: Option<Vec<u8>>,
}

/// The codec a track's current sample entry names.
enum Entry {
    Avc {
        params: ParameterSets,
        prefix: Arc<[u8]>,
    },
    Aac(Arc<[u8]>),
}

/// What one init sets up. A new init means a new demuxer: it accepts one init only.
struct InitScope {
    demuxer: Fmp4SegmentDemuxer,
    offsets: HashMap<u32, i64>,
    entries: HashMap<u32, Entry>,
    /// Video tracks that met their first sample under this init.
    started: HashSet<u32>,
}

impl InitScope {
    fn open(init: &[u8]) -> anyhow::Result<Self> {
        if init.len() > MAX_INIT_BYTES {
            bail!(
                "an init of {} bytes, past the ceiling of {MAX_INIT_BYTES}",
                init.len()
            );
        }
        let mut demuxer = Fmp4SegmentDemuxer::new();
        demuxer
            .handle_init_segment(init)
            .map_err(|e| anyhow!("the init segment does not demux: {e}"))?;
        Ok(Self {
            demuxer,
            offsets: edit_offsets(init)?,
            entries: HashMap::new(),
            started: HashSet::new(),
        })
    }
}

/// The preroll is read for its last AAC frame only; its video is neither converted nor copied.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Pass {
    Preroll,
    Media,
}

#[derive(Default)]
struct Units {
    video: Vec<VideoUnit>,
    audio: Vec<AudioUnit>,
    video_track: Option<u32>,
    audio_track: Option<u32>,
    /// Every distinct parameter-set prefix and audio config of the request, held once each.
    prefixes: HashSet<Arc<[u8]>>,
    configs: HashSet<Arc<[u8]>>,
}

/// Every `moof` + `mdat` pair of the append, in order, a new init taking over where it appears.
pub(crate) fn demux(req: &FragmentRequest) -> anyhow::Result<DemuxedFragment> {
    let mut scope = if req.init.is_empty() {
        None
    } else {
        Some(InitScope::open(req.init)?)
    };
    let mut budget = SampleBudget::new("video fragment", MAX_FRAGMENT_SAMPLES, req.media.len());
    let mut units = Units::default();
    demux_buffer(req.media, &mut scope, Pass::Media, &mut budget, &mut units)?;
    if units.video.is_empty() && units.audio.is_empty() {
        bail!("the fragment carries no samples");
    }
    let preroll_audio = preroll_frame(req, units.audio.first())?;
    Ok(DemuxedFragment {
        video: units.video,
        audio: units.audio,
        preroll_audio,
    })
}

/// The preroll's last AAC frame, kept only under the config of the append's first frame: the
/// decoder it primes is the one that frame opens.
fn preroll_frame(
    req: &FragmentRequest,
    first: Option<&AudioUnit>,
) -> anyhow::Result<Option<Vec<u8>>> {
    let Some(first) = first else {
        return Ok(None);
    };
    if req.preroll.is_empty() || req.init.is_empty() {
        return Ok(None);
    }
    let mut scope = Some(InitScope::open(req.init)?);
    let mut budget = SampleBudget::new("preroll", MAX_FRAGMENT_SAMPLES, req.preroll.len());
    let mut units = Units::default();
    demux_buffer(
        req.preroll,
        &mut scope,
        Pass::Preroll,
        &mut budget,
        &mut units,
    )?;
    Ok(units
        .audio
        .pop()
        .filter(|last| last.asc == first.asc)
        .map(|last| last.data))
}

fn demux_buffer(
    data: &[u8],
    scope: &mut Option<InitScope>,
    pass: Pass,
    budget: &mut SampleBudget,
    units: &mut Units,
) -> anyhow::Result<()> {
    let mut init_start = None;
    let mut spans = top_level_boxes(data);
    while let Some(span) = spans.next().transpose()? {
        match &span.kind {
            b"ftyp" => init_start = Some(span.start),
            b"moov" => {
                let start = init_start.take().unwrap_or(span.start);
                let init = data
                    .get(start..span.end)
                    .ok_or_else(|| anyhow!("an init lies outside its buffer"))?;
                *scope = Some(InitScope::open(init)?);
            }
            b"moof" => {
                let mdat = spans
                    .next()
                    .transpose()?
                    .filter(|next| &next.kind == b"mdat")
                    .ok_or_else(|| anyhow!("a moof is not followed by its mdat"))?;
                let pair = data
                    .get(span.start..mdat.end)
                    .ok_or_else(|| anyhow!("a moof and mdat lie outside their buffer"))?;
                let scope = scope
                    .as_mut()
                    .ok_or_else(|| anyhow!("the fragment's media comes before any init"))?;
                let moof = box_body(data, &span)?;
                demux_pair(pair, moof, scope, pass, budget, units)?;
            }
            // `styp`, `sidx`, `prft`, `emsg`, `free`: nothing to decode.
            _ => {}
        }
    }
    Ok(())
}

/// The samples a `moof`'s runs declare. Counted before the crate parses them: a `trun` without
/// per-sample fields declares up to 16M samples in a few bytes, and the crate allocates each one.
fn declared_samples(moof: &[u8]) -> anyhow::Result<usize> {
    let mut total = 0usize;
    for traf in top_level_boxes(moof) {
        let traf = traf?;
        if &traf.kind != b"traf" {
            continue;
        }
        let traf = box_body(moof, &traf)?;
        for trun in top_level_boxes(traf) {
            let trun = trun?;
            if &trun.kind != b"trun" {
                continue;
            }
            // Version and flags, then `sample_count`.
            let count = box_body(traf, &trun)?
                .get(4..)
                .and_then(|b| b.first_chunk::<4>())
                .ok_or_else(|| anyhow!("a trun is cut short"))?;
            let count = usize::try_from(u32::from_be_bytes(*count))
                .map_err(|_| anyhow!("a trun's sample count does not fit memory"))?;
            total = total.saturating_add(count);
        }
    }
    Ok(total)
}

fn demux_pair(
    pair: &[u8],
    moof: &[u8],
    scope: &mut InitScope,
    pass: Pass,
    budget: &mut SampleBudget,
    units: &mut Units,
) -> anyhow::Result<()> {
    if moof.len() > MAX_MOOF_BYTES {
        bail!(
            "a moof of {} bytes, past the ceiling of {MAX_MOOF_BYTES}",
            moof.len()
        );
    }
    let declared = declared_samples(moof)?;
    if declared > MAX_FRAGMENT_SAMPLES {
        bail!("a moof declares {declared} samples, past the ceiling of {MAX_FRAGMENT_SAMPLES}");
    }
    let InitScope {
        demuxer,
        offsets,
        entries,
        started,
    } = scope;
    let samples = demuxer
        .handle_media_segment(pair)
        .map_err(|e| anyhow!("a moof and mdat do not demux: {e}"))?;
    for sample in &samples {
        let track_id = sample.track.track_id;
        // Present on a track's first sample, then only when its sample entry changes.
        match sample.sample_entry {
            Some(SampleEntry::Avc1(avc1)) => {
                let params = parameter_sets(&avc1.avcc_box)?;
                let prefix = intern(&mut units.prefixes, params.prefix()?);
                entries.insert(track_id, Entry::Avc { params, prefix });
            }
            Some(SampleEntry::Mp4a(mp4a)) => {
                let asc = intern(&mut units.configs, mp4a_config(mp4a)?);
                entries.insert(track_id, Entry::Aac(asc));
            }
            Some(_) => bail!("the fragment carries a codec other than H.264 or AAC"),
            None => {}
        }
        let entry = entries
            .get(&track_id)
            .ok_or_else(|| anyhow!("a sample arrived before its sample entry"))?;

        // Charged first: a run pointing every sample at one region is refused before it is copied.
        budget.charge(sample.data_size)?;
        let bytes = sample_bytes(pair, sample.data_offset, sample.data_size)
            .ok_or_else(|| anyhow!("a sample lies outside its mdat"))?;
        let edit = offsets.get(&track_id).copied().unwrap_or(0);
        let pts = presentation_time(sample.timestamp, sample.composition_time_offset, edit)?;
        let timescale = sample.track.timescale.get();

        match entry {
            Entry::Avc { .. } if pass == Pass::Preroll => {}
            Entry::Avc { params, prefix } => {
                same_track(&mut units.video_track, track_id, "video")?;
                let mut annex_b = Vec::new();
                let idr = annex_b_access_unit(bytes, params, &mut annex_b)?;
                // Each append opens a fresh decoder, and only an IDR slice starts one.
                if started.insert(track_id) && !idr {
                    bail!("the fragment does not start on an IDR slice");
                }
                units.video.push(VideoUnit {
                    annex_b,
                    idr,
                    prefix: prefix.clone(),
                    pts,
                    duration: sample.duration,
                    timescale,
                });
            }
            Entry::Aac(asc) => {
                same_track(&mut units.audio_track, track_id, "audio")?;
                units.audio.push(AudioUnit {
                    data: bytes.to_vec(),
                    pts,
                    duration: sample.duration,
                    timescale,
                    asc: asc.clone(),
                });
            }
        }
    }
    Ok(())
}

/// The AudioSpecificConfig of an MPEG-4 audio entry, AAC-LC only and at most `MAX_ASC_BYTES`.
fn mp4a_config(mp4a: &Mp4aBox) -> anyhow::Result<Vec<u8>> {
    let config = &mp4a.esds_box.es.dec_config_descr;
    if config.object_type_indication
        != DecoderConfigDescriptor::OBJECT_TYPE_INDICATION_AUDIO_ISO_IEC_14496_3
    {
        bail!("the audio is not MPEG-4 audio");
    }
    let asc = &config
        .dec_specific_info
        .as_ref()
        .ok_or_else(|| anyhow!("the audio carries no AudioSpecificConfig"))?
        .payload;
    if asc.len() > MAX_ASC_BYTES {
        bail!(
            "the audio's AudioSpecificConfig is {} bytes, past the cap of {MAX_ASC_BYTES}",
            asc.len()
        );
    }
    aac_lc(asc)?;
    Ok(asc.clone())
}

/// One decoder per kind: a second video or audio track would interleave two streams into it.
fn same_track(seen: &mut Option<u32>, track_id: u32, kind: &str) -> anyhow::Result<()> {
    match *seen.get_or_insert(track_id) {
        id if id == track_id => Ok(()),
        _ => bail!("the fragment carries more than one {kind} track"),
    }
}

/// DTS + composition offset - edit offset, as Chromium times the sample. Refused rather than
/// wrapped: a wrapped time looks plausible and is wrong.
fn presentation_time(dts: u64, offset: Option<i64>, edit: i64) -> anyhow::Result<i64> {
    i64::try_from(dts)
        .ok()
        .and_then(|dts| dts.checked_add(offset.unwrap_or(0)))
        .and_then(|pts| pts.checked_sub(edit))
        .ok_or_else(|| anyhow!("a sample's presentation time overflows"))
}

#[cfg(test)]
#[path = "../../../tests/unit/video/fragment/demux.rs"]
mod tests;
