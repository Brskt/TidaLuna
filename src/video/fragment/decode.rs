//! A demuxed append decoded one record at a time: each AAC frame to f32 PCM, then each H.264
//! frame to packed I420, in display order.

use std::collections::VecDeque;
use std::sync::Arc;

use anyhow::{anyhow, bail};
use openh264::decoder::{DecodedYUV, Decoder};
use openh264::formats::YUVSource;
use symphonia::core::codecs::audio::well_known::CODEC_ID_AAC;
use symphonia::core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia::core::packet::PacketRef;
use symphonia::core::units::{Duration, Timestamp};
use symphonia::default::codecs::AacDecoder;

use crate::video::cover::access_unit;
use crate::video::deadline::Deadline;

use super::demux::{AudioUnit, DemuxedFragment, VideoUnit};

const VIDEO_RECORD: u8 = 0;
const AUDIO_RECORD: u8 = 1;

/// The tag, then timescale, pts, duration, width and height.
const VIDEO_HEADER_LEN: usize = 1 + 4 + 8 + 4 + 4 + 4;

/// The tag, then timescale, pts, duration, sample rate, channels and frames.
const AUDIO_HEADER_LEN: usize = 1 + 4 + 8 + 4 + 4 + 4 + 4;

/// One sample's time, handed to the frame that comes out in its place.
struct FrameTime {
    pts: i64,
    duration: u32,
    timescale: u32,
}

/// A fragment's records, decoded as they are asked for. No thread of its own: it moves to the
/// pool thread CEF reads on, and `next_record` is all of its work.
pub(crate) struct FragmentDecode {
    audio: std::vec::IntoIter<AudioUnit>,
    aac: Option<(Arc<[u8]>, AacDecoder)>,
    preroll: Option<Vec<u8>>,
    pcm: Vec<f32>,
    video: std::vec::IntoIter<VideoUnit>,
    /// The access unit being decoded, refilled for each sample.
    scratch: Vec<u8>,
    decoder: Option<Decoder>,
    /// The samples' presentation times, sorted: the decoder returns none usable, and frames come
    /// out in display order, each taking the next one.
    times: VecDeque<FrameTime>,
    ready: VecDeque<Vec<u8>>,
    drained: bool,
    frame_count: usize,
    /// One `read` may run many decode calls, and CEF delivers a cancel only after it returns.
    deadline: Deadline,
}

impl FragmentDecode {
    /// A fresh H.264 decoder per append, since `demux` makes every one start on an IDR slice.
    pub(crate) fn open(
        frag: DemuxedFragment,
        api: openh264::OpenH264API,
        deadline: Deadline,
    ) -> anyhow::Result<Self> {
        let decoder = if frag.video.is_empty() {
            None
        } else {
            Some(
                crate::video::codec::start_decoder(api)
                    .map_err(|e| anyhow!("starting the H.264 decoder failed: {e}"))?,
            )
        };
        let mut times: Vec<FrameTime> = frag
            .video
            .iter()
            .map(|unit| FrameTime {
                pts: unit.pts,
                duration: unit.duration,
                timescale: unit.timescale,
            })
            .collect();
        times.sort_by_key(|t| t.pts);
        Ok(Self {
            audio: frag.audio.into_iter(),
            aac: None,
            preroll: frag.preroll_audio,
            pcm: Vec::new(),
            frame_count: frag.video.len(),
            video: frag.video.into_iter(),
            scratch: Vec::new(),
            decoder,
            times: times.into(),
            ready: VecDeque::new(),
            drained: false,
            deadline,
        })
    }

    /// The next record over `out`'s previous contents; `false`, with `out` empty, at the end.
    /// One frame out per sample in, or it fails: a frame without a time would mistime the rest.
    pub(crate) fn next_record(&mut self, out: &mut Vec<u8>) -> anyhow::Result<bool> {
        out.clear();
        if let Some(unit) = self.audio.next() {
            self.audio_record(&unit, out)?;
            return Ok(true);
        }
        loop {
            if let Some(record) = self.ready.pop_front() {
                *out = record;
                return Ok(true);
            }
            if !self.decode_more()? {
                if !self.times.is_empty() {
                    bail!(
                        "the fragment decoded {} frames for {} samples",
                        self.frame_count - self.times.len(),
                        self.frame_count
                    );
                }
                return Ok(false);
            }
        }
    }

    fn audio_record(&mut self, unit: &AudioUnit, out: &mut Vec<u8>) -> anyhow::Result<()> {
        let decoder = match &mut self.aac {
            Some((asc, decoder)) if asc[..] == unit.asc[..] => decoder,
            slot => {
                let mut decoder = aac_decoder(&unit.asc)?;
                if let Some(frame) = self.preroll.take() {
                    prime(&mut decoder, &frame);
                }
                &mut slot.insert((unit.asc.clone(), decoder)).1
            }
        };
        let packet = PacketRef::new(
            0,
            Timestamp::new(unit.pts),
            Duration::new(u64::from(unit.duration)),
            &unit.data,
        );
        let decoded = decoder
            .decode_ref(&packet)
            .map_err(|e| anyhow!("decoding an AAC frame failed: {e}"))?;
        let rate = decoded.spec().rate();
        let channels = u32::try_from(decoded.spec().channels().count())?;
        let frames = u32::try_from(decoded.frames())?;
        decoded.copy_to_vec_interleaved(&mut self.pcm);

        out.reserve(AUDIO_HEADER_LEN + self.pcm.len() * 4);
        out.push(AUDIO_RECORD);
        out.extend_from_slice(&unit.timescale.to_le_bytes());
        out.extend_from_slice(&unit.pts.to_le_bytes());
        out.extend_from_slice(&unit.duration.to_le_bytes());
        out.extend_from_slice(&rate.to_le_bytes());
        out.extend_from_slice(&channels.to_le_bytes());
        out.extend_from_slice(&frames.to_le_bytes());
        for sample in &self.pcm {
            out.extend_from_slice(&sample.to_le_bytes());
        }
        Ok(())
    }

    /// Feed the decoder one sample, or drain it once the samples are spent. False when there is
    /// nothing left to feed or drain.
    fn decode_more(&mut self) -> anyhow::Result<bool> {
        let Self {
            video,
            scratch,
            decoder,
            times,
            ready,
            drained,
            deadline,
            ..
        } = self;
        let Some(decoder) = decoder.as_mut() else {
            return Ok(false);
        };
        if !*drained {
            deadline.check()?;
        }
        if let Some(unit) = video.next() {
            access_unit(&unit.prefix, unit.idr, &unit.annex_b, scratch);
            let decoded = decoder
                .decode(scratch)
                .map_err(|e| anyhow!("decoding an H.264 frame failed: {e}"))?;
            // `None` is ordinary: the decoder holds frames back until it can reorder them.
            if let Some(yuv) = decoded {
                ready.push_back(video_record(&yuv, times)?);
            }
            return Ok(true);
        }
        if *drained {
            return Ok(false);
        }
        *drained = true;
        // The pictures held back for reordering only come out here.
        let remaining = decoder
            .flush_remaining()
            .map_err(|e| anyhow!("draining the H.264 decoder failed: {e}"))?;
        for yuv in &remaining {
            ready.push_back(video_record(yuv, times)?);
        }
        Ok(true)
    }
}

fn aac_decoder(asc: &[u8]) -> anyhow::Result<AacDecoder> {
    let mut params = AudioCodecParameters::new();
    params.for_codec(CODEC_ID_AAC).with_extra_data(asc.into());
    AacDecoder::try_new(&params, &AudioDecoderOptions::default())
        .map_err(|e| anyhow!("the AAC decoder refused its config: {e}"))
}

/// Decode the previous fragment's last frame and drop its output: the first frame's MDCT overlap
/// comes from it, and without it a boundary clicks. A frame that fails costs that click only.
fn prime(decoder: &mut AacDecoder, frame: &[u8]) {
    let packet = PacketRef::new(0, Timestamp::new(0), Duration::new(1024), frame);
    if let Err(e) = decoder.decode_ref(&packet) {
        crate::vprintln!("[VIDEO]   Video fragment preroll did not decode: {e}");
        decoder.reset();
    }
}

/// One frame's record: its sample's time, its size, then Y, U and V packed without row padding.
fn video_record(yuv: &DecodedYUV<'_>, times: &mut VecDeque<FrameTime>) -> anyhow::Result<Vec<u8>> {
    let time = times
        .pop_front()
        .ok_or_else(|| anyhow!("the fragment decoded more frames than it has samples"))?;
    let (width, height) = yuv.dimensions();
    // The record's chroma planes are half the size each way: an odd size has no exact half.
    if width == 0 || height == 0 || width % 2 != 0 || height % 2 != 0 {
        bail!("the fragment decoded a {width}x{height} frame, not an even size");
    }
    let (chroma_width, chroma_height) = (width / 2, height / 2);
    let planes = width
        .checked_mul(height)
        .and_then(|luma| luma.checked_add(luma / 2))
        .ok_or_else(|| anyhow!("a {width}x{height} frame does not fit memory"))?;
    let (y_stride, u_stride, v_stride) = yuv.strides();

    let mut out = Vec::with_capacity(VIDEO_HEADER_LEN + planes);
    out.push(VIDEO_RECORD);
    out.extend_from_slice(&time.timescale.to_le_bytes());
    out.extend_from_slice(&time.pts.to_le_bytes());
    out.extend_from_slice(&time.duration.to_le_bytes());
    out.extend_from_slice(&u32::try_from(width)?.to_le_bytes());
    out.extend_from_slice(&u32::try_from(height)?.to_le_bytes());
    copy_plane(yuv.y(), y_stride, width, height, &mut out)?;
    copy_plane(yuv.u(), u_stride, chroma_width, chroma_height, &mut out)?;
    copy_plane(yuv.v(), v_stride, chroma_width, chroma_height, &mut out)?;
    Ok(out)
}

/// `rows` rows of `width` bytes, dropping the padding the decoder puts past each.
fn copy_plane(
    plane: &[u8],
    stride: usize,
    width: usize,
    rows: usize,
    out: &mut Vec<u8>,
) -> anyhow::Result<()> {
    if stride < width {
        bail!("a decoded plane's stride {stride} is narrower than its width {width}");
    }
    for row in 0..rows {
        let line = row
            .checked_mul(stride)
            .and_then(|start| plane.get(start..start.checked_add(width)?))
            .ok_or_else(|| anyhow!("a decoded plane is shorter than its size"))?;
        out.extend_from_slice(line);
    }
    Ok(())
}
