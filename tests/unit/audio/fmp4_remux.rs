//! Tests for `src/audio/fmp4_remux.rs`, attached by `#[path]`.

use super::*;
use crate::player::dash_fixture::{AAC_FRAME, Durations, SEGMENTS, dash_track_primed};
use shiguredo_mp4::demux::Mp4FileDemuxer;

/// Fills every `mdat` of `file` with bytes that differ per position, and returns them in file
/// order: the fixture's zero filler could not tell one sample from another.
fn stamp_payloads(file: &mut [u8]) -> Vec<u8> {
    let mut payload = Vec::new();
    let mut at = 0;
    while at + 8 <= file.len() {
        let size = u32::from_be_bytes(file[at..at + 4].try_into().unwrap()) as usize;
        if &file[at + 4..at + 8] == b"mdat" {
            for (i, byte) in file[at + 8..at + size].iter_mut().enumerate() {
                *byte = (at + i) as u8 ^ 0x5a;
                payload.push(*byte);
            }
        }
        at += size;
    }
    payload
}

/// The remux must be invisible to the listener: every sample's bytes in order, the same total
/// length, and the encoder delay the source declared, all of which a player would get wrong
/// without an error. The tags travel in `moov` untouched.
#[test]
fn a_remux_keeps_every_sample_the_length_and_the_encoder_delay() {
    const PRIMING: u32 = 2048;
    let mut source = dash_track_primed(&SEGMENTS, Durations::PerSample, Some(PRIMING));
    let payload = stamp_payloads(&mut source);
    let udta = b"\0\0\0\x08free".to_vec();

    let out = to_progressive(&source, Some(udta.clone())).unwrap();

    let mut demuxer = Mp4FileDemuxer::new();
    demuxer.handle_input(Input {
        position: 0,
        data: &out,
    });
    let mut copied = Vec::new();
    let mut ticks = 0u64;
    let mut count = 0usize;
    while let Some(sample) = demuxer.next_sample().unwrap() {
        copied.extend_from_slice(sample_bytes(&out, sample.data_offset, sample.data_size).unwrap());
        ticks += u64::from(sample.duration);
        count += 1;
    }
    let frames: u32 = SEGMENTS.iter().sum();
    assert_eq!(copied, payload, "sample bytes");
    assert_eq!(count, frames as usize);
    assert_eq!(ticks, u64::from(frames * AAC_FRAME));

    let (moov, _) = MoovBox::decode(top_level_box(&out, b"moov").unwrap()).unwrap();
    let movie = u64::from(moov.mvhd_box.timescale.get());
    let media = u64::from(moov.trak_boxes[0].mdia_box.mdhd_box.timescale.get());
    let presentation = ((ticks - u64::from(PRIMING)) * movie + media / 2) / media;
    let edits: Vec<(u64, i64)> = moov.trak_boxes[0]
        .edts_box
        .as_ref()
        .and_then(|edts| edts.elst_box.as_ref())
        .map(|elst| {
            elst.entries
                .iter()
                .map(|e| (e.edit_duration, e.media_time))
                .collect()
        })
        .unwrap_or_default();
    assert_eq!(edits, [(presentation, i64::from(PRIMING))]);
    assert_eq!(moov.mvhd_box.duration, presentation);
    assert_eq!(moov.trak_boxes[0].tkhd_box.duration, presentation);
    let udtas: Vec<&[u8]> = moov
        .unknown_boxes
        .iter()
        .filter(|b| b.box_type == BoxType::Normal(*b"udta"))
        .map(|b| b.payload.as_slice())
        .collect();
    assert_eq!(udtas, [udta.as_slice()]);
}
