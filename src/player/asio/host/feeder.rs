use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::mpsc;

use symphonia::core::codecs::CodecParameters;
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::formats::FormatOptions;
use symphonia::core::formats::probe::Hint;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;

use super::AsioCommand;
use crate::player::buffer::RamBuffer;
use crate::player::packet_failure::{PacketFailure, classify_packet_failure};
use crate::player::throttle::{DECODE_AHEAD_SECS, throttle_decode_ahead};

/// Decode a `RamBuffer` (the same source the cpal/exclusive paths read) to
/// interleaved i32 and feed the ASIO control thread via `AsioCommand`. Mirrors
/// `wasapi::stream_flac_reader_to_wasapi`, but the ring is i32; `PushPcm` carries
/// `Vec<i32>` directly (no byte-packing) and the RT host renders it as src_bps=32.
/// `RamBuffer` is itself a `MediaSource`, needing no `SizedMediaSource` wrapper.
#[expect(clippy::too_many_arguments)]
pub(crate) fn stream_reader_to_asio(
    reader: RamBuffer,
    stream_id: u32,
    opened_under: u32,
    cmd_tx: mpsc::Sender<AsioCommand>,
    cancel: Arc<AtomicBool>,
    seek_to: Option<f64>,
    // Identity of the initial seek, minted by the spawn alongside `stream_id`; the live
    // seeks that follow carry their own on the channel.
    seek_gen_id: u32,
    start_paused: bool,
    seek_rx: mpsc::Receiver<(f64, u32)>,
    consumed: Arc<AtomicU64>,
) -> Result<(), String> {
    // A second reader over the same bytes, for the length a header may leave out.
    let whole = reader.clone();
    let mss = MediaSourceStream::new(Box::new(reader), Default::default());

    let mut hint = Hint::new();
    hint.with_extension("flac");

    // These fail before the control thread adopts the stream, so nothing downstream hears them
    // on their own: `DecodeFailed` is reserved for a stream already adopted. Silent when the
    // spawn was superseded, a retirement being no failure. The cause is the caller's to supply,
    // only the probe reading bytes and only it able to classify before `format!` erases the type.
    let startup_failed = |error: String, cause: PacketFailure| -> String {
        if !cancel.load(Ordering::Relaxed) {
            let _ = cmd_tx.send(AsioCommand::StartupFailed {
                stream_id,
                error: error.clone(),
                cause,
            });
        }
        error
    };

    let mut format_reader = symphonia::default::get_probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        // The one call here that touches bytes, and the only one that can name the connection.
        .map_err(|e| startup_failed(format!("probe failed: {e}"), classify_packet_failure(&e)))?;

    // Everything past the probe reads what it already holds: the bytes arrived and say
    // something unusable, which is the source's fault however the connection behaved.
    let track = format_reader
        .tracks()
        .iter()
        .find(|t| matches!(&t.codec_params, Some(CodecParameters::Audio(_))))
        .ok_or_else(|| startup_failed("no audio track".to_string(), PacketFailure::Source))?
        .clone();

    let codec_params = match &track.codec_params {
        Some(CodecParameters::Audio(p)) => p,
        _ => {
            return Err(startup_failed(
                "no audio track".to_string(),
                PacketFailure::Source,
            ));
        }
    };
    let sample_rate = codec_params
        .sample_rate
        .ok_or_else(|| startup_failed("no sample rate".to_string(), PacketFailure::Source))?;
    let channels = codec_params
        .channels
        .as_ref()
        .ok_or_else(|| startup_failed("no channel info".to_string(), PacketFailure::Source))?
        .count() as u32;
    let duration_secs = crate::player::length::track_secs(&track, sample_rate, &whole);

    // StartStream FIRST at offset 0 makes the control thread open the driver while a
    // forward seek into not-yet-downloaded data resolves; the landing position
    // follows via ResetForSeek (mirrors the exclusive path).
    cmd_tx
        .send(AsioCommand::StartStream {
            stream_id,
            opened_under,
            sample_rate,
            channels,
            duration_secs,
            start_secs: 0.0,
            start_paused,
            consumed: consumed.clone(),
        })
        .map_err(|_| "failed to send StartStream".to_string())?;

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
                    crate::vprintln!("[ASIO] decoder seek to {t:.1}s (actual {actual:.1}s)");
                    if cmd_tx
                        .send(AsioCommand::ResetForSeek {
                            stream_id,
                            gen_id: seek_gen_id,
                            start_secs: actual,
                        })
                        .is_err()
                    {
                        return Ok(());
                    }
                }
                Err(e) => {
                    crate::vprintln!("[ASIO] decoder seek to {t:.1}s failed, will retry: {e}");
                    pending_initial_seek = Some((t, seek_gen_id));
                }
            }
        } else {
            // A target that `Time` cannot represent will not become representable: refuse it
            // rather than arm a retry that can only fail the same way.
            crate::vprintln!("[ASIO] decoder seek to {t:.1}s is out of range");
            was_initial_seek = false;
            if cmd_tx
                .send(AsioCommand::SeekFailed {
                    stream_id,
                    gen_id: seek_gen_id,
                })
                .is_err()
            {
                return Ok(());
            }
        }
    }

    // Past StartStream, the control thread already holds this stream and `DecodeFailed`
    // is the right carrier. The progress watchdog would eventually notice and fall back to
    // shared, but only after its two seconds of silence, and only here: the exclusive twin
    // has no watchdog at all. Reporting at the source beats waiting for either.
    let mut decoder = match symphonia::default::get_codecs()
        .make_audio_decoder(codec_params, &AudioDecoderOptions::default())
    {
        Ok(decoder) => decoder,
        Err(e) => {
            let error = format!("decoder creation failed: {e}");
            if !cancel.load(Ordering::Relaxed) {
                let _ = cmd_tx.send(AsioCommand::DecodeFailed {
                    stream_id,
                    error: error.clone(),
                    cause: PacketFailure::Source,
                });
            }
            return Err(error);
        }
    };

    // Back-pressure target in interleaved samples (matching the ring): decoding
    // further ahead races the streaming download window and stalls the read.
    let throttle_hi = sample_rate as u64 * channels as u64 * DECODE_AHEAD_SECS;
    // Interleaved samples sent so far, throttled against `consumed`.
    let mut sent: u64 = 0;

    loop {
        if cancel.load(Ordering::Relaxed) {
            return Ok(());
        }

        if throttle_decode_ahead(
            sent,
            &consumed,
            throttle_hi,
            &cancel,
            &seek_rx,
            &mut pending_initial_seek,
            &mut was_initial_seek,
        ) {
            return Ok(());
        }

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
            crate::vprintln!("[ASIO] live seek to {t:.1}s is out of range");
            pending_seek = None;
            was_initial_seek = false;
            if cmd_tx
                .send(AsioCommand::SeekFailed { stream_id, gen_id })
                .is_err()
            {
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
                    crate::vprintln!("[ASIO] live seek to {t:.1}s (actual {actual:.1}s)");
                    if cmd_tx
                        .send(AsioCommand::ResetForSeek {
                            stream_id,
                            gen_id,
                            start_secs: actual,
                        })
                        .is_err()
                    {
                        return Ok(());
                    }
                }
                Err(e) => {
                    crate::vprintln!("[ASIO] live seek to {t:.1}s failed: {e}");
                    if was_initial_seek {
                        pending_initial_seek = Some((t, gen_id));
                    } else if cmd_tx
                        .send(AsioCommand::SeekFailed { stream_id, gen_id })
                        .is_err()
                    {
                        return Ok(());
                    }
                }
            }
        }

        let packet = match format_reader.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) => {
                if cancel.load(Ordering::Relaxed) {
                    crate::vprintln!("[ASIO] decode: cancelled (stream {stream_id})");
                    return Ok(());
                }
                // Real EOF: arm completion, then PARK on the seek channel instead of
                // returning. Dropping seek_rx would kill live seeks for the rest of the
                // track (a cached source decodes far ahead of playback while the control
                // thread still owns the buffered tail). ResetForSeek un-ends the stream on
                // a later seek, re-arming completion after it.
                crate::vprintln!("[ASIO] decode: EOF (stream {stream_id}), parked for seeks");
                let _ = cmd_tx.send(AsioCommand::EndStream { stream_id });
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
                if cancel.load(Ordering::Relaxed) {
                    return Ok(());
                }
                // Classified here and nowhere later: this is the last point the error is
                // still typed, and a stall is indistinguishable from a bad frame once both
                // are a String.
                let cause = classify_packet_failure(&e);
                let error = format!("decode packet error: {e}");
                // Returning alone tells nobody: the caller only logs, and the player would
                // keep a seek channel this thread is about to drop.
                let _ = cmd_tx.send(AsioCommand::DecodeFailed {
                    stream_id,
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

        let mut samples: Vec<i32> = Vec::new();
        decoded.copy_to_vec_interleaved::<i32>(&mut samples);

        let sample_count = samples.len() as u64;
        if !samples.is_empty()
            && cmd_tx
                .send(AsioCommand::PushPcm { stream_id, samples })
                .is_err()
        {
            return Ok(());
        }
        sent += sample_count;
    }
}
