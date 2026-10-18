//! Tests for `src/player/length.rs`, attached to it by `#[path]`.

use super::dash_fixture::{Durations, SAMPLE_RATE, SEGMENTS, dash_track, seconds};
use super::{declared_secs, packet_ticks, track_secs};
use crate::player::buffer::RamBuffer;
use symphonia::core::formats::Track;
use symphonia::core::units::{Duration, TimeBase};

/// What the container's own probe makes of `bytes`: the first track, as the player reads it.
fn first_track(bytes: &[u8]) -> Track {
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::formats::probe::Hint;
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;
    let mss = MediaSourceStream::new(
        Box::new(RamBuffer::from_complete(bytes.to_vec())),
        Default::default(),
    );
    let format = symphonia::default::get_probe()
        .probe(
            &Hint::new(),
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .expect("the fixture is a valid fragmented MP4");
    format.tracks()[0].clone()
}

#[test]
fn a_dash_track_is_measured_from_its_fragments() {
    for (frames, how) in [
        (&SEGMENTS[..], Durations::PerSample),
        (&SEGMENTS[..], Durations::Default),
        (&[3][..], Durations::PerSample),
    ] {
        let bytes = dash_track(frames, how);
        let whole = RamBuffer::from_complete(bytes.clone());
        let got = track_secs(&first_track(&bytes), SAMPLE_RATE, &whole);
        assert!(
            (got - seconds(frames)).abs() < 1e-6,
            "{got} s for {} s of frames",
            seconds(frames)
        );
    }
}

#[test]
fn a_track_still_arriving_is_not_measured() {
    // A read past the frontier parks until the bytes land: counting packets over a source that is
    // not whole would hold the start of playback behind the download.
    let bytes = dash_track(&SEGMENTS, Durations::PerSample);
    let track = first_track(&bytes);
    let (arriving, writer) = RamBuffer::new_for_test(bytes.len() as u64);
    assert!(writer.write_counted(&bytes[..bytes.len() / 2]));
    let began = std::time::Instant::now();
    assert_eq!(track_secs(&track, SAMPLE_RATE, &arriving), 0.0);
    // The read that parks gives up after 30 s with the same answer: only the time tells.
    assert!(
        began.elapsed() < std::time::Duration::from_secs(5),
        "the pass waited on bytes that were not there"
    );
}

#[test]
fn the_pass_leaves_the_read_cursor_where_it_found_it() {
    // One word shared by every reader of the buffer, which the load's progress reports read.
    use std::io::{Seek, SeekFrom};
    let bytes = dash_track(&SEGMENTS, Durations::PerSample);
    let whole = RamBuffer::from_complete(bytes.clone());
    whole.clone().seek(SeekFrom::Start(7)).unwrap();
    assert!(track_secs(&first_track(&bytes), SAMPLE_RATE, &whole) > 0.0);
    assert_eq!(whole.read_cursor(), 7);
}

/// Bytes that stop being readable after `limit` of them, as a download that dies mid-file does.
struct DiesAfter {
    inner: std::io::Cursor<Vec<u8>>,
    limit: u64,
}

impl std::io::Read for DiesAfter {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.inner.position() >= self.limit {
            return Err(std::io::Error::other("the source stopped"));
        }
        let room = (self.limit - self.inner.position()) as usize;
        let take = buf.len().min(room);
        self.inner.read(&mut buf[..take])
    }
}

impl std::io::Seek for DiesAfter {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        self.inner.seek(pos)
    }
}

impl symphonia::core::io::MediaSource for DiesAfter {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        None
    }
}

#[test]
fn a_read_that_fails_midway_gives_no_length_rather_than_a_short_one() {
    // The packets counted before the failure would add up to less than the track's length.
    let bytes = dash_track(&SEGMENTS, Durations::PerSample);
    let track = first_track(&bytes);
    let whole = packet_ticks(
        DiesAfter {
            inner: std::io::Cursor::new(bytes.clone()),
            limit: u64::MAX,
        },
        track.id,
    );
    assert!(
        whole.is_some_and(|t| t > 0),
        "the untruncated source counts"
    );

    let midway = (bytes.len() * 2 / 3) as u64;
    let cut = packet_ticks(
        DiesAfter {
            inner: std::io::Cursor::new(bytes),
            limit: midway,
        },
        track.id,
    );
    assert_eq!(cut, None);
}

fn declared(num_frames: Option<u64>, duration: Option<u64>, timescale: u32) -> Track {
    let mut track = Track::new(1);
    track.num_frames = num_frames;
    track.duration = duration.map(Duration::from);
    track.time_base = TimeBase::try_from_recip(timescale);
    track
}

#[test]
fn a_frame_count_is_the_length() {
    let track = declared(Some(441_000), Some(1), 44_100);
    assert_eq!(declared_secs(&track, SAMPLE_RATE), 10.0);
}

#[test]
fn a_duration_in_another_timescale_is_the_length() {
    // An MP4 whose media timescale is not the sample rate has no frame count in symphonia, and
    // its declared duration is the only length it has.
    let track = declared(None, Some(220_500), 22_050);
    assert_eq!(declared_secs(&track, SAMPLE_RATE), 10.0);
}

#[test]
fn a_zero_is_no_declaration() {
    assert_eq!(
        declared_secs(&declared(Some(0), Some(0), 44_100), SAMPLE_RATE),
        0.0
    );
    assert_eq!(
        declared_secs(&declared(None, None, 44_100), SAMPLE_RATE),
        0.0
    );
    assert_eq!(declared_secs(&declared(Some(100), None, 44_100), 0), 0.0);
}
