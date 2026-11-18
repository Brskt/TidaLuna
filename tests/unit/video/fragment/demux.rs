//! Tests for `src/video/fragment/demux.rs`, attached by `#[path]`.

use super::*;

use std::num::NonZeroU32;

use shiguredo_mp4::boxes::{
    Avc1Box, AvccBox, EdtsBox, ElstBox, ElstEntry, MoovBox, SampleEntry, VisualSampleEntryFields,
};
use shiguredo_mp4::mux::{Fmp4SegmentMuxer, Sample};
use shiguredo_mp4::{Decode, Encode, FixedPointNumber, TrackKind, Uint};

const TIMESCALE: u32 = 24_000;

/// One frame at 23.976 fps, in `TIMESCALE` units.
const FRAME: u32 = 1_001;

fn avc1() -> SampleEntry {
    SampleEntry::Avc1(Avc1Box {
        visual: VisualSampleEntryFields {
            data_reference_index: VisualSampleEntryFields::DEFAULT_DATA_REFERENCE_INDEX,
            width: 16,
            height: 16,
            horizresolution: VisualSampleEntryFields::DEFAULT_HORIZRESOLUTION,
            vertresolution: VisualSampleEntryFields::DEFAULT_VERTRESOLUTION,
            frame_count: VisualSampleEntryFields::DEFAULT_FRAME_COUNT,
            compressorname: [0; 32],
            depth: VisualSampleEntryFields::DEFAULT_DEPTH,
        },
        avcc_box: AvccBox {
            avc_profile_indication: 0x42,
            profile_compatibility: 0x00,
            avc_level_indication: 0x1E,
            length_size_minus_one: Uint::new(3),
            // Baseline, level 3.0, one macroblock: 16x16 like the entry.
            sps_list: vec![vec![0x67, 0x42, 0x00, 0x1E, 0xDA, 0x79]],
            pps_list: vec![vec![0x68, 0xCE]],
            chroma_format: None,
            bit_depth_luma_minus8: None,
            bit_depth_chroma_minus8: None,
            sps_ext_list: Vec::new(),
        },
        unknown_boxes: Vec::new(),
    })
}

/// One video sample of a fixture: a sync sample or not, its composition offset, and its one
/// length-prefixed slice NAL.
#[derive(Clone, Copy)]
struct Frame {
    key: bool,
    cts: Option<i64>,
    nal: &'static [u8],
}

/// A sync sample holding an IDR slice, and a plain one holding a non-IDR slice.
const KEY: Frame = Frame {
    key: true,
    cts: None,
    nal: &[0, 0, 0, 2, 0x65, 0x88],
};
const PLAIN: Frame = Frame {
    key: false,
    cts: None,
    nal: &[0, 0, 0, 2, 0x41, 0x9A],
};

/// One `moof` + `mdat` pair holding `frames`, timed after every pair `muxer` wrote before it.
fn pair(muxer: &mut Fmp4SegmentMuxer, frames: &[Frame]) -> Vec<u8> {
    let mut payload = Vec::new();
    let samples: Vec<Sample> = frames
        .iter()
        .map(|f| {
            let data_offset = payload.len() as u64;
            payload.extend_from_slice(f.nal);
            Sample {
                track_kind: TrackKind::Video,
                sample_entry: Some(avc1()),
                keyframe: f.key,
                timescale: NonZeroU32::new(TIMESCALE).expect("non-zero"),
                duration: FRAME,
                composition_time_offset: f.cts,
                data_offset,
                data_size: f.nal.len(),
            }
        })
        .collect();
    let mut out = muxer
        .create_media_segment_metadata(&samples)
        .expect("moof and mdat header");
    out.extend_from_slice(&payload);
    out
}

/// `init` with a one-entry edit list starting its first track at `media_time`.
fn with_edit_list(init: &[u8], media_time: i64) -> Vec<u8> {
    let moov = top_level_boxes(init)
        .map(|b| b.expect("init boxes"))
        .find(|b| &b.kind == b"moov")
        .expect("moov");
    let (mut parsed, _) = MoovBox::decode(&init[moov.start..moov.end]).expect("moov decodes");
    parsed.trak_boxes[0].edts_box = Some(EdtsBox {
        elst_box: Some(ElstBox {
            entries: vec![ElstEntry {
                edit_duration: 0,
                media_time,
                media_rate: FixedPointNumber::new(1, 0),
            }],
        }),
        unknown_boxes: Vec::new(),
    });
    let mut out = init[..moov.start].to_vec();
    out.extend_from_slice(&parsed.encode_to_vec().expect("moov encodes"));
    out
}

fn request<'a>(init: &'a [u8], media: &'a [u8]) -> FragmentRequest<'a> {
    FragmentRequest {
        init,
        preroll: &[],
        media,
    }
}

#[test]
fn every_moof_mdat_pair_of_an_append_is_demuxed() {
    let mut muxer = Fmp4SegmentMuxer::new().expect("muxer");
    let first = pair(&mut muxer, &[KEY, PLAIN]);
    let second = pair(&mut muxer, &[KEY, PLAIN]);
    let init = muxer.init_segment_bytes().expect("init");
    let media = [first, second].concat();

    let fragment = demux(&request(&init, &media)).expect("demuxed");
    let pts: Vec<i64> = fragment.video.iter().map(|u| u.pts).collect();
    assert_eq!(pts, [0, 1_001, 2_002, 3_003]);
    assert!(
        fragment
            .video
            .iter()
            .all(|u| u.timescale == TIMESCALE && u.duration == FRAME)
    );
}

#[test]
fn the_edit_list_shifts_presentation_times() {
    let mut muxer = Fmp4SegmentMuxer::new().expect("muxer");
    let cts = Some(1_001);
    let media = pair(&mut muxer, &[Frame { cts, ..KEY }, Frame { cts, ..PLAIN }]);
    let init = with_edit_list(&muxer.init_segment_bytes().expect("init"), 2_002);

    let fragment = demux(&request(&init, &media)).expect("demuxed");
    let pts: Vec<i64> = fragment.video.iter().map(|u| u.pts).collect();
    // dts + cts - media_time: 0 + 1001 - 2002, then 1001 + 1001 - 2002.
    assert_eq!(pts, [-1_001, 0]);
}

#[test]
fn audio_other_than_aac_lc_is_refused() {
    // AAC-LC, 44.1 kHz, stereo: accepted. Each refusal below changes one field of it.
    assert!(aac_lc(&[0x12, 0x10]).is_ok());
    // AAC-LC signalling SBR in an extension (sync 0x2B7, type 5, flag 1): a decoder that ignores it
    // plays at half rate without a word.
    assert!(aac_lc(&[0x12, 0x10, 0x56, 0xE5, 0x80]).is_err());
    // AAC-LC, 44.1 kHz, channel configuration 6 (5.1).
    assert!(aac_lc(&[0x12, 0x30]).is_err());
}

/// The container's sync flag is the muxer's word; only an IDR slice starts a fresh decoder.
#[test]
fn a_sync_flagged_sample_without_an_idr_slice_is_refused() {
    let mut muxer = Fmp4SegmentMuxer::new().expect("muxer");
    let media = pair(
        &mut muxer,
        &[
            Frame {
                nal: PLAIN.nal,
                ..KEY
            },
            KEY,
        ],
    );
    let init = muxer.init_segment_bytes().expect("init");

    let refused = demux(&request(&init, &media)).err().expect("refused");
    assert!(refused.to_string().contains("IDR"), "{refused:#}");
}

fn boxed(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let size = u32::try_from(8 + body.len()).expect("a test box fits a 32-bit size");
    let mut out = size.to_be_bytes().to_vec();
    out.extend_from_slice(kind);
    out.extend_from_slice(body);
    out
}

/// Any trusted frame may post here, and the crate allocates many times the bytes of an init or a
/// `moof` it decodes: past their ceilings both are refused before it sees them.
#[test]
fn an_init_or_a_moof_past_its_ceiling_is_refused() {
    let mut muxer = Fmp4SegmentMuxer::new().expect("muxer");
    let media = pair(&mut muxer, &[KEY]);
    let init = muxer.init_segment_bytes().expect("init");

    let mut padded = init.clone();
    padded.extend_from_slice(&boxed(b"free", &vec![0; MAX_INIT_BYTES]));
    let refused = demux(&request(&padded, &media))
        .err()
        .expect("init refused");
    assert!(refused.to_string().contains("an init of"), "{refused:#}");

    let mut moof = boxed(b"moof", &boxed(b"free", &vec![0; MAX_MOOF_BYTES]));
    moof.extend_from_slice(&boxed(b"mdat", &[]));
    let refused = demux(&request(&init, &moof)).err().expect("moof refused");
    assert!(refused.to_string().contains("a moof of"), "{refused:#}");
}
