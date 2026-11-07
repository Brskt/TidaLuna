//! A downloaded DASH track, as TIDAL serves it (an init segment then media segments, which
//! concatenate into a fragmented MP4), rewritten as a progressive MP4: one `moov` describing every
//! sample, as players and taggers expect of a music file.
//!
//! The samples are copied untouched; only the container changes.

use crate::util::mp4_samples::{SampleBudget, sample_bytes};
use shiguredo_mp4::boxes::{EdtsBox, ElstBox, ElstEntry, MoovBox, UnknownBox};
use shiguredo_mp4::demux::{Fmp4FileDemuxer, Input};
use shiguredo_mp4::mux::{Mp4FileMuxer, Mp4FileMuxerOptions, Sample as MuxSample};
use shiguredo_mp4::{BoxSize, BoxType, Decode, Encode, FixedPointNumber};

/// Ten hours of 1024-sample AAC frames at 48 kHz is 1.69 million. Bounds what the `trun` boxes may
/// declare; the byte ceiling alone would let a tiny sample size ask for one entry per byte.
const MAX_TRACK_SAMPLES: usize = 2_000_000;

/// Rewrite `fmp4` as a progressive MP4 with `moov` at the end, carrying `udta_body` (the tags) when
/// given. Refuses a file with more than one track: TIDAL's carry one, and copying a second into
/// the first's sample table would corrupt both.
pub(crate) fn to_progressive(fmp4: &[u8], udta_body: Option<Vec<u8>>) -> anyhow::Result<Vec<u8>> {
    let priming = source_priming(fmp4)?;

    let mut demuxer = Fmp4FileDemuxer::new();
    let mut muxer = Mp4FileMuxer::with_options(Mp4FileMuxerOptions::default())
        .map_err(|e| anyhow::anyhow!("MP4 muxer refused its options: {e}"))?;
    let mut head = muxer.initial_boxes_bytes().to_vec();
    let mut budget = SampleBudget::new("DASH track", MAX_TRACK_SAMPLES, fmp4.len());
    let mut track_id = None;
    // Where the samples lie in the input, one run per contiguous stretch. They are copied once,
    // after the loop, into an output allocated at its final size instead of grown by doubling.
    let mut runs: Vec<(usize, usize)> = Vec::new();
    let mut samples_end = head.len();

    loop {
        // Fed the range it asks for, never the whole buffer: a media segment handed everything
        // from its `moof` to the end of the file reads the following segments as trailing data.
        while let Some(required) = demuxer.required_input() {
            let start = usize::try_from(required.position).unwrap_or(usize::MAX);
            let end = required
                .size
                .map_or(fmp4.len(), |size| start.saturating_add(size));
            let data = fmp4.get(start..end.min(fmp4.len())).unwrap_or(&[]);
            demuxer.handle_input(Input {
                position: required.position,
                data,
            });
        }
        let Some(sample) = demuxer
            .next_sample()
            .map_err(|e| anyhow::anyhow!("DASH track is not a readable fragmented MP4: {e}"))?
        else {
            break;
        };
        if *track_id.get_or_insert(sample.track.track_id) != sample.track.track_id {
            anyhow::bail!("DASH track carries more than one track");
        }

        budget.charge(sample.data_size)?;
        sample_bytes(fmp4, sample.data_offset, sample.data_size)
            .ok_or_else(|| anyhow::anyhow!("DASH sample is empty or lies outside the file"))?;
        let start = usize::try_from(sample.data_offset)?;
        let end = start + sample.data_size;
        match runs.last_mut() {
            Some(run) if run.1 == start => run.1 = end,
            _ => runs.push((start, end)),
        }
        // The muxer records where each sample starts and requires them contiguous, in order.
        let data_offset = samples_end as u64;
        samples_end += sample.data_size;
        muxer
            .append_sample(&MuxSample {
                track_kind: sample.track.kind,
                sample_entry: sample.sample_entry.cloned(),
                keyframe: sample.keyframe,
                timescale: sample.track.timescale,
                duration: sample.duration,
                composition_time_offset: sample.composition_time_offset,
                data_offset,
                data_size: sample.data_size,
            })
            .map_err(|e| anyhow::anyhow!("MP4 muxer refused a sample: {e}"))?;
    }
    if track_id.is_none() {
        anyhow::bail!("DASH track carries no samples");
    }

    let finalized = muxer
        .finalize()
        .map_err(|e| anyhow::anyhow!("MP4 muxer could not finish the file: {e}"))?;
    if finalized.is_faststart_enabled() {
        anyhow::bail!("MP4 muxer placed moov before the samples, which this does not patch");
    }
    // Everything but `moov` lies in front of the samples: it patches the head, never the data.
    for (offset, bytes) in finalized.offset_and_bytes_pairs() {
        let start = usize::try_from(offset)?;
        if start == samples_end {
            continue;
        }
        let end = start + bytes.len();
        anyhow::ensure!(end <= head.len(), "MP4 muxer wrote past its head");
        head[start..end].copy_from_slice(bytes);
    }

    let mut moov = finalized.moov_box().clone();
    if let Some(media_time) = priming {
        restore_priming(&mut moov, media_time);
    }
    if let Some(body) = udta_body {
        let box_size = u32::try_from(body.len() + 8)
            .map_err(|_| anyhow::anyhow!("tags of {} bytes exceed a 32-bit box", body.len()))?;
        moov.unknown_boxes.push(UnknownBox {
            box_type: BoxType::Normal(*b"udta"),
            box_size: BoxSize::U32(box_size),
            payload: body,
        });
    }
    let moov = moov
        .encode_to_vec()
        .map_err(|e| anyhow::anyhow!("could not encode the moov box: {e}"))?;

    // `moov` ends the file; growing it moves no sample, leaving every chunk offset valid.
    let mut out = Vec::with_capacity(samples_end + moov.len());
    out.extend_from_slice(&head);
    for (start, end) in runs {
        out.extend_from_slice(&fmp4[start..end]);
    }
    out.extend_from_slice(&moov);
    Ok(out)
}

/// The encoder delay the source declares: the first edit's `media_time`, in media timescale, when
/// it skips samples. The muxer writes no edit list; without it a player would play the AAC
/// priming samples as sound. Read as upstream reads it.
fn source_priming(fmp4: &[u8]) -> anyhow::Result<Option<i64>> {
    let moov = top_level_box(fmp4, b"moov")
        .ok_or_else(|| anyhow::anyhow!("DASH track has no moov box"))?;
    let (moov, _) = MoovBox::decode(moov)
        .map_err(|e| anyhow::anyhow!("DASH track's moov box is unreadable: {e}"))?;
    let media_time = moov
        .trak_boxes
        .first()
        .and_then(|trak| trak.edts_box.as_ref())
        .and_then(|edts| edts.elst_box.as_ref())
        .and_then(|elst| elst.entries.first())
        .map(|entry| entry.media_time);
    Ok(media_time.filter(|t| *t > 0))
}

/// One edit starting `media_time` into the media and lasting the rest of it, with the movie and
/// track durations shortened to match, as upstream's `patchMoov` writes it. Left out when that
/// span is empty or does not fit the 32-bit fields upstream writes.
fn restore_priming(moov: &mut MoovBox, media_time: i64) {
    let movie_timescale = u128::from(moov.mvhd_box.timescale.get());
    let Some(trak) = moov.trak_boxes.first_mut() else {
        return;
    };
    let mdhd = &trak.mdia_box.mdhd_box;
    let media_timescale = u128::from(mdhd.timescale.get());
    let Some(span) = u128::from(mdhd.duration).checked_sub(media_time as u128) else {
        return;
    };
    let presentation = (span * movie_timescale + media_timescale / 2) / media_timescale;
    let Ok(presentation) = u32::try_from(presentation) else {
        return;
    };
    if presentation == 0 {
        return;
    }
    let presentation = u64::from(presentation);
    trak.edts_box = Some(EdtsBox {
        elst_box: Some(ElstBox {
            entries: vec![ElstEntry {
                edit_duration: presentation,
                media_time,
                media_rate: FixedPointNumber::new(1, 0),
            }],
        }),
        unknown_boxes: vec![],
    });
    trak.tkhd_box.duration = presentation;
    moov.mvhd_box.duration = presentation;
}

/// The first top-level box of `kind`, header included, or `None` when the chain breaks first.
fn top_level_box<'a>(data: &'a [u8], kind: &[u8; 4]) -> Option<&'a [u8]> {
    let mut at = 0usize;
    while let Some(header) = data.get(at..at.checked_add(8)?) {
        let size = match u32::from_be_bytes(header[..4].try_into().ok()?) {
            0 => data.len() - at,
            1 => usize::try_from(u64::from_be_bytes(
                data.get(at + 8..at + 16)?.try_into().ok()?,
            ))
            .ok()?,
            size => size as usize,
        };
        let end = at
            .checked_add(size)
            .filter(|end| *end <= data.len() && size >= 8)?;
        if &header[4..8] == kind {
            return Some(&data[at..end]);
        }
        at = end;
    }
    None
}

#[cfg(test)]
#[path = "../../tests/unit/audio/fmp4_remux.rs"]
mod tests;
