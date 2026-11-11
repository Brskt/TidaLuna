//! The decode loop the exclusive-WASAPI and ASIO engines share: probe, initial seek, throttled
//! decode, live seeks and the EOF park. Each backend's feeder supplies a [`FeedSink`] for its
//! command channel and its sample format.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::mpsc;

use symphonia::core::audio::GenericAudioBufferRef;
use symphonia::core::codecs::CodecParameters;
use symphonia::core::codecs::audio::{AudioCodecParameters, AudioDecoderOptions};
use symphonia::core::formats::FormatOptions;
use symphonia::core::formats::probe::Hint;
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::MetadataOptions;

use crate::player::buffer::RamBuffer;
use crate::player::packet_failure::{PacketFailure, classify_packet_failure};
use crate::player::throttle::{DECODE_AHEAD_SECS, throttle_decode_ahead};

/// What the loop tells the backend; each sink maps it onto its own command enum.
pub(super) enum Feed {
    StartupFailed {
        error: String,
        cause: PacketFailure,
    },
    StartStream {
        sample_rate: u32,
        channels: u32,
        duration_secs: f64,
        start_secs: f64,
    },
    ResetForSeek {
        gen_id: u32,
        start_secs: f64,
    },
    SeekFailed {
        gen_id: u32,
    },
    DecodeFailed {
        error: String,
        cause: PacketFailure,
    },
    EndStream,
}

/// The backend's command channel is gone: nobody is left to feed.
pub(super) struct Closed;

pub(super) trait FeedSink {
    /// Prefix of the loop's log lines.
    const TAG: &'static str;

    /// The stream this feed opened, for the log lines.
    fn stream_id(&self) -> u32;

    /// Reads what the backend needs from the track before `StartStream`; an `Err` is a
    /// startup failure of the source.
    fn adopt_format(&mut self, params: &AudioCodecParameters) -> Result<(), String>;

    /// What `consumed` counts per interleaved sample, the throttle's unit.
    fn units_per_sample(&self) -> u64;

    fn send(&self, feed: Feed) -> Result<(), Closed>;

    /// The count returned is in interleaved samples; the loop scales it by `units_per_sample`.
    fn push(&mut self, decoded: GenericAudioBufferRef<'_>) -> Result<u64, Closed>;
}

// These fail before the backend adopts the stream, so nothing downstream hears them on their
// own: `DecodeFailed` is reserved for a stream already held. Silent when the spawn was
// superseded, a retirement being no failure. The cause is the caller's to supply, only the
// probe reading bytes and only it able to classify before `format!` erases the type.
fn startup_failed<S: FeedSink>(
    sink: &S,
    cancel: &AtomicBool,
    error: String,
    cause: PacketFailure,
) -> String {
    if !cancel.load(Relaxed) {
        let _ = sink.send(Feed::StartupFailed {
            error: error.clone(),
            cause,
        });
    }
    error
}

pub(super) fn run<S: FeedSink>(
    mut sink: S,
    source: Box<dyn MediaSource>,
    // A second reader over the same bytes, for the length a header may leave out.
    whole: &RamBuffer,
    cancel: &AtomicBool,
    seek_to: Option<f64>,
    // Identity of the initial seek, minted by the spawn alongside `stream_id`; the live
    // seeks that follow carry their own on the channel.
    seek_gen_id: u32,
    seek_rx: &mpsc::Receiver<(f64, u32)>,
    consumed: &AtomicU64,
) -> Result<(), String> {
    let tag = S::TAG;
    let mss = MediaSourceStream::new(source, Default::default());

    let mut hint = Hint::new();
    hint.with_extension("flac");

    let mut format_reader = symphonia::default::get_probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        // The one call here that touches bytes, and the only one that can name the connection.
        .map_err(|e| {
            startup_failed(
                &sink,
                cancel,
                format!("probe failed: {e}"),
                classify_packet_failure(&e),
            )
        })?;

    // Everything past the probe reads what it already holds: the bytes arrived and say
    // something unusable, which is the source's fault however the connection behaved.
    let source_failed = |sink: &S, error: &str| {
        startup_failed(sink, cancel, error.to_string(), PacketFailure::Source)
    };
    let track = format_reader
        .tracks()
        .iter()
        .find(|t| matches!(&t.codec_params, Some(CodecParameters::Audio(_))))
        .ok_or_else(|| source_failed(&sink, "no audio track"))?
        .clone();

    let codec_params = match &track.codec_params {
        Some(CodecParameters::Audio(p)) => p,
        _ => return Err(source_failed(&sink, "no audio track")),
    };
    let sample_rate = codec_params
        .sample_rate
        .ok_or_else(|| source_failed(&sink, "no sample rate"))?;
    let channels = codec_params
        .channels
        .as_ref()
        .ok_or_else(|| source_failed(&sink, "no channel info"))?
        .count() as u32;
    if let Err(error) = sink.adopt_format(codec_params) {
        return Err(source_failed(&sink, &error));
    }
    let duration_secs = crate::player::length::track_secs(&track, sample_rate, whole);

    // StartStream FIRST at offset 0 makes the backend adopt this stream_id and open the
    // device immediately, rather than staying silent while a forward seek into
    // not-yet-downloaded data blocks below. The real landing position follows via
    // ResetForSeek (which needs the stream_id this sets).
    sink.send(Feed::StartStream {
        sample_rate,
        channels,
        duration_secs,
        start_secs: 0.0,
    })
    .map_err(|_| "failed to send StartStream".to_string())?;

    // Source seek AFTER StartStream, letting a forward seek past the buffered PCM move
    // (mirrors do_decode_seek). On success re-base the backend via ResetForSeek; on
    // failure (offset not yet downloaded) seed an initial seek the decode loop
    // retries, rather than playing from 0.
    let mut pending_initial_seek: Option<(f64, u32)> = None;
    let mut was_initial_seek = false;
    if let Some(t) = seek_to
        && t > 0.0
    {
        was_initial_seek = true;
        if let Some(time_pos) = symphonia::core::units::Time::try_from_secs_f64(t) {
            match format_reader.seek(
                symphonia::core::formats::SeekMode::Coarse,
                symphonia::core::formats::SeekTo::Time {
                    time: time_pos,
                    track_id: Some(track.id),
                },
            ) {
                Ok(seeked) => {
                    // Settled: a later live-seek failure must not re-arm an initial retry.
                    was_initial_seek = false;
                    let actual = if sample_rate > 0 {
                        seeked.actual_ts.get() as f64 / sample_rate as f64
                    } else {
                        t
                    };
                    crate::vprintln!("{tag} decoder seek to {t:.1}s (actual {actual:.1}s)");
                    if sink
                        .send(Feed::ResetForSeek {
                            gen_id: seek_gen_id,
                            start_secs: actual,
                        })
                        .is_err()
                    {
                        return Ok(());
                    }
                }
                Err(e) => {
                    crate::vprintln!("{tag} decoder seek to {t:.1}s failed, will retry: {e}");
                    pending_initial_seek = Some((t, seek_gen_id));
                }
            }
        } else {
            // A target that `Time` cannot represent will not become representable: refuse it
            // rather than arm a retry that can only fail the same way.
            crate::vprintln!("{tag} decoder seek to {t:.1}s is out of range");
            was_initial_seek = false;
            if sink
                .send(Feed::SeekFailed {
                    gen_id: seek_gen_id,
                })
                .is_err()
            {
                return Ok(());
            }
        }
    }

    // Past StartStream, the backend already holds this stream and `DecodeFailed` is the
    // right carrier. Unreported, the device stays open on a stream that never yields a
    // sample: ASIO's watchdog notices only after two seconds of silence, exclusive never.
    let mut decoder = match symphonia::default::get_codecs()
        .make_audio_decoder(codec_params, &AudioDecoderOptions::default())
    {
        Ok(decoder) => decoder,
        Err(e) => {
            let error = format!("decoder creation failed: {e}");
            if !cancel.load(Relaxed) {
                let _ = sink.send(Feed::DecodeFailed {
                    error: error.clone(),
                    cause: PacketFailure::Source,
                });
            }
            return Err(error);
        }
    };

    // Back-pressure target in the unit `consumed` counts: unthrottled, a cached source
    // decodes the whole track into RAM at once, and a streaming one races its download.
    let units_per_sample = sink.units_per_sample();
    let throttle_hi = sample_rate as u64 * channels as u64 * units_per_sample * DECODE_AHEAD_SECS;
    // Units sent so far, throttled against `consumed`.
    let mut sent: u64 = 0;

    loop {
        if cancel.load(Relaxed) {
            return Ok(());
        }

        if throttle_decode_ahead(
            sent,
            consumed,
            throttle_hi,
            cancel,
            seek_rx,
            &mut pending_initial_seek,
            &mut was_initial_seek,
        ) {
            return Ok(());
        }

        // Apply the pending seek in place (no re-probe), mirroring do_decode_seek.
        // Start from a seeded initial seek, then let a newer live seek win. On
        // success the backend flushes between old and new PCM (channel order is the
        // epoch); a failed initial seek re-seeds for a later retry.
        let mut pending_seek = pending_initial_seek.take();
        while let Ok(t) = seek_rx.try_recv() {
            pending_seek = Some(t);
            was_initial_seek = false;
        }
        // Answered here rather than in the chain below: a failed conversion makes that
        // chain fall through as a whole, dropping the seek with no reply at all.
        if let Some((t, gen_id)) = pending_seek
            && symphonia::core::units::Time::try_from_secs_f64(t).is_none()
        {
            crate::vprintln!("{tag} live seek to {t:.1}s is out of range");
            pending_seek = None;
            was_initial_seek = false;
            if sink.send(Feed::SeekFailed { gen_id }).is_err() {
                return Ok(());
            }
        }
        if let Some((t, gen_id)) = pending_seek
            && let Some(time_pos) = symphonia::core::units::Time::try_from_secs_f64(t)
        {
            match format_reader.seek(
                symphonia::core::formats::SeekMode::Coarse,
                symphonia::core::formats::SeekTo::Time {
                    time: time_pos,
                    track_id: Some(track.id),
                },
            ) {
                Ok(seeked) => {
                    decoder.reset();
                    was_initial_seek = false;
                    let actual = if sample_rate > 0 {
                        seeked.actual_ts.get() as f64 / sample_rate as f64
                    } else {
                        t
                    };
                    crate::vprintln!("{tag} live seek to {t:.1}s (actual {actual:.1}s)");
                    if sink
                        .send(Feed::ResetForSeek {
                            gen_id,
                            start_secs: actual,
                        })
                        .is_err()
                    {
                        return Ok(());
                    }
                }
                Err(e) => {
                    crate::vprintln!("{tag} live seek to {t:.1}s failed: {e}");
                    if was_initial_seek {
                        pending_initial_seek = Some((t, gen_id));
                    } else if sink.send(Feed::SeekFailed { gen_id }).is_err() {
                        return Ok(());
                    }
                }
            }
        }

        let packet = match format_reader.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) => {
                let stream_id = sink.stream_id();
                if cancel.load(Relaxed) {
                    crate::vprintln!("{tag} decode: cancelled (stream {stream_id})");
                    return Ok(());
                }
                // Real EOF: arm completion, then PARK on the seek channel instead of
                // returning. Dropping seek_rx would kill live seeks for the rest of the
                // track (a cached source decodes far ahead of playback while the backend
                // still owns the buffered tail). ResetForSeek un-ends the stream on a
                // later seek, re-arming completion after it.
                crate::vprintln!("{tag} decode: EOF (stream {stream_id}), parked for seeks");
                let _ = sink.send(Feed::EndStream);
                // Disconnect is the ONLY exit while parked: every supersede path
                // stores `cancel` then drops the sender, and natural completion
                // just drops it. Either way nobody can seek this stream again.
                match seek_rx.recv() {
                    Ok(t) => {
                        pending_initial_seek = Some(t);
                        was_initial_seek = false;
                        continue;
                    }
                    Err(_) => return Ok(()),
                }
            }
            Err(e) => {
                if cancel.load(Relaxed) {
                    return Ok(());
                }
                // Classified here and nowhere later: this is the last point the error is
                // still typed, and a stall is indistinguishable from a bad frame once both
                // are a String.
                let cause = classify_packet_failure(&e);
                let error = format!("decode packet error: {e}");
                // Returning alone tells nobody: the caller only logs, and the player would
                // keep a seek channel this thread is about to drop.
                let _ = sink.send(Feed::DecodeFailed {
                    error: error.clone(),
                    cause,
                });
                return Err(error);
            }
        };

        if packet.track_id != track.id {
            continue;
        }

        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(_) => continue,
        };

        match sink.push(decoded) {
            Ok(samples) => sent += samples * units_per_sample,
            Err(Closed) => return Ok(()),
        }
    }
}

#[cfg(test)]
#[path = "../../tests/unit/player/decode_feed.rs"]
mod tests;
