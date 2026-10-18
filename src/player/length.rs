//! A container either declares its length or leaves it out. A fragmented MP4, which is what a DASH
//! track concatenates to, declares none. Its header is written before the fragments exist (the
//! `mdhd` and `mvhd` lengths read 0), and the fragments carry the real count. A 0 reported for a
//! track that has a length reads as "unknown" everywhere downstream: the page does not settle the
//! SDK's reload of the track on it, the crossfade never arms, the position is not clamped, the
//! bitrate reads 0 and the OS media controls show no length.

use crate::player::buffer::RamBuffer;
use std::io::{Seek, SeekFrom};
use symphonia::core::formats::Track;
use symphonia::core::formats::probe::Hint;
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::units::Duration;

/// What the container declares for `track`, in seconds; 0 when it declares none.
///
/// The frame count first, being what the decoder yields once delay and padding are cut. Its
/// absence is no verdict on the length: an MP4 whose media timescale is not the sample rate has
/// no frame count (symphonia's isomp4 sets it only when the two are equal) but still declares a
/// duration in its own timescale. What a header declares is trusted: counting the packets of
/// every whole file would cost a pass over a FLAC for a length its header already holds.
pub(crate) fn declared_secs(track: &Track, sample_rate: u32) -> f64 {
    if let Some(frames) = track.num_frames
        && frames > 0
        && sample_rate > 0
    {
        return frames as f64 / sample_rate as f64;
    }
    match (track.duration, track.time_base) {
        (Some(duration), Some(base)) if duration.get() > 0 => base
            .calc_duration(duration)
            .map_or(0.0, |time| time.as_secs_f64()),
        _ => 0.0,
    }
}

/// What the packets of `track` add up to in `source`, in seconds; `None` when they cannot be
/// counted.
///
/// A second reader over the same bytes, run to its end; each packet is read, none is decoded.
/// Refused for a `source` still arriving: a read past the frontier parks until the bytes land, and
/// this runs ahead of the start of playback.
fn measured_secs(source: &RamBuffer, track: &Track) -> Option<f64> {
    if !source.is_complete() {
        return None;
    }
    // The read cursor is one word shared by every reader of the buffer, and the pass leaves it at
    // the end of the file: put back where the caller had it.
    let cursor = source.read_cursor();
    let ticks = packet_ticks(source.clone(), track.id).filter(|&t| t > 0);
    let _ = source.clone().seek(SeekFrom::Start(cursor));
    let time = track.time_base?.calc_duration(Duration::from(ticks?))?;
    Some(time.as_secs_f64())
}

/// The frames of track `track_id` that `source`'s packets add up to, in the track's own ticks.
///
/// Counted to the end of the stream, never to a read that failed: a source that stops mid-way
/// has no length, and a short sum reported as one would be believed.
fn packet_ticks(source: impl MediaSource + 'static, track_id: u32) -> Option<u64> {
    let mss = MediaSourceStream::new(Box::new(source), Default::default());
    let mut format = symphonia::default::get_probe()
        .probe(
            &Hint::new(),
            mss,
            symphonia::core::formats::FormatOptions::default(),
            symphonia::core::meta::MetadataOptions::default(),
        )
        .ok()?;
    let mut ticks: u64 = 0;
    // The frames the decoder yields before any trimming, which is what this player plays: it cuts
    // nothing.
    while let Some(packet) = format.next_packet().ok()? {
        if packet.track_id != track_id {
            continue;
        }
        ticks = ticks
            .checked_add(packet.dur.get())?
            .checked_add(packet.trim_start.get())?
            .checked_add(packet.trim_end.get())?;
    }
    Some(ticks)
}

/// The length of `track` in seconds: what its container declares, else what its packets add up
/// to when `source` holds the whole file; 0 when neither can say.
///
/// One answer for every backend. A length counted here and not there would reach the page as
/// known on one device and unknown on the next.
pub(crate) fn track_secs(track: &Track, sample_rate: u32, source: &RamBuffer) -> f64 {
    let declared = declared_secs(track, sample_rate);
    if declared > 0.0 {
        return declared;
    }
    measured_secs(source, track).unwrap_or(0.0)
}

#[cfg(test)]
#[path = "../../tests/unit/player/dash_fixture.rs"]
pub(crate) mod dash_fixture;

#[cfg(test)]
#[path = "../../tests/unit/player/length.rs"]
mod tests;
