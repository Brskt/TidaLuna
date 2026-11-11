use std::io::{Read, Seek};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, mpsc};

use symphonia::core::codecs::CodecParameters;
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::formats::FormatOptions;
use symphonia::core::formats::probe::Hint;
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::MetadataOptions;

use super::{ExclusiveCommand, StreamStart};
use crate::player::packet_failure::{PacketFailure, classify_packet_failure};
use crate::player::throttle::{DECODE_AHEAD_SECS, throttle_decode_ahead};

struct SizedMediaSource {
    inner: crate::player::buffer::RamBuffer,
    byte_len: u64,
}

impl Read for SizedMediaSource {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.inner.read(buf)
    }
}

impl Seek for SizedMediaSource {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        self.inner.seek(pos)
    }
}

impl MediaSource for SizedMediaSource {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        Some(self.byte_len)
    }
}

fn append_interleaved_i32_as_pcm(samples: &[i32], bits_per_sample: u32, out: &mut Vec<u8>) {
    if bits_per_sample <= 16 {
        // i32 samples -> take upper 16 bits (little-endian: bytes [2..4]).
        out.reserve(samples.len() * 2);
        for &s in samples {
            let b = s.to_le_bytes();
            out.push(b[2]);
            out.push(b[3]);
        }
    } else if bits_per_sample <= 24 {
        // i32 samples -> take upper 24 bits (bytes [1..4]).
        out.reserve(samples.len() * 3);
        for &s in samples {
            let b = s.to_le_bytes();
            out.push(b[1]);
            out.push(b[2]);
            out.push(b[3]);
        }
    } else {
        // 32-bit: pass through all 4 bytes.
        out.reserve(samples.len() * 4);
        for &s in samples {
            out.extend_from_slice(&s.to_le_bytes());
        }
    }
}

#[expect(clippy::too_many_arguments)]
pub(in crate::player) fn stream_flac_reader_to_wasapi(
    reader: crate::player::buffer::RamBuffer,
    byte_len: u64,
    stream_id: u32,
    opened_under: u32,
    cmd_tx: mpsc::Sender<ExclusiveCommand>,
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
    let source = Box::new(SizedMediaSource {
        inner: reader,
        byte_len,
    });
    let mss = MediaSourceStream::new(source, Default::default());

    crate::vprintln!("[WASAPI-DBG] flac reader: starting probe (byte_len={byte_len})");
    let mut hint = Hint::new();
    hint.with_extension("flac");

    // These fail before the render adopts the stream, so nothing downstream hears them on their
    // own: `DecodeFailed` is reserved for a stream already held. Silent when the spawn was
    // superseded, a retirement being no failure. The cause is the caller's to supply, only the
    // probe reading bytes and only it able to classify before `format!` erases the type.
    let startup_failed = |error: String, cause: PacketFailure| -> String {
        if !cancel.load(Relaxed) {
            let _ = cmd_tx.send(ExclusiveCommand::StartupFailed {
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
    let bits_per_sample = codec_params
        .bits_per_sample
        .ok_or_else(|| startup_failed("no bits_per_sample".to_string(), PacketFailure::Source))?;
    let duration_secs = crate::player::length::track_secs(&track, sample_rate, &whole);

    let stored_bps = if bits_per_sample <= 16 {
        16
    } else if bits_per_sample <= 24 {
        24
    } else {
        32
    };

    crate::vprintln!(
        "[WASAPI-DBG] flac reader: probe OK ({sample_rate}Hz {channels}ch {stored_bps}bit), sending StartStream"
    );
    // StartStream FIRST at offset 0 makes the render adopt this stream_id and open
    // the client immediately, rather than staying silent while a forward seek into
    // not-yet-downloaded data blocks below. The real landing position follows via
    // ResetForSeek (which needs the stream_id this sets).
    cmd_tx
        .send(ExclusiveCommand::StartStream(StreamStart {
            stream_id,
            opened_under,
            sample_rate,
            channels,
            bits_per_sample: stored_bps,
            duration_secs,
            start_secs: 0.0,
            start_paused,
            consumed: consumed.clone(),
        }))
        .map_err(|_| "failed to send StartStream".to_string())?;

    // Source seek AFTER StartStream, letting a forward seek past the buffered PCM move
    // (mirrors do_decode_seek). On success re-base the render via ResetForSeek; on
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
                    crate::vprintln!("[WASAPI] decoder seek to {t:.1}s (actual {actual:.1}s)");
                    if cmd_tx
                        .send(ExclusiveCommand::ResetForSeek {
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
                    crate::vprintln!("[WASAPI] decoder seek to {t:.1}s failed, will retry: {e}");
                    pending_initial_seek = Some((t, seek_gen_id));
                }
            }
        } else {
            // A target that `Time` cannot represent will not become representable: refuse it
            // rather than arm a retry that can only fail the same way.
            crate::vprintln!("[WASAPI] decoder seek to {t:.1}s is out of range");
            was_initial_seek = false;
            if cmd_tx
                .send(ExclusiveCommand::SeekFailed {
                    stream_id,
                    gen_id: seek_gen_id,
                })
                .is_err()
            {
                return Ok(());
            }
        }
    }

    // Past StartStream, the render already holds this stream and `DecodeFailed` is the
    // right carrier here. The idle arm's premise, that a decoder-sent command follows its
    // own adoption, holds. Unreported this left the endpoint open on a stream that would
    // never yield a sample; ASIO's progress watchdog catches that, exclusive has none.
    let mut decoder = match symphonia::default::get_codecs()
        .make_audio_decoder(codec_params, &AudioDecoderOptions::default())
    {
        Ok(decoder) => decoder,
        Err(e) => {
            let error = format!("decoder creation failed: {e}");
            if !cancel.load(Relaxed) {
                let _ = cmd_tx.send(ExclusiveCommand::DecodeFailed {
                    stream_id,
                    error: error.clone(),
                    cause: PacketFailure::Source,
                });
            }
            return Err(error);
        }
    };

    // Back-pressure target in unplayed PCM bytes (ASIO's is in samples):
    // unthrottled, a cached source decodes the whole track into RAM at once.
    let throttle_hi =
        sample_rate as u64 * channels as u64 * (stored_bps as u64 / 8) * DECODE_AHEAD_SECS;
    // Source-packed bytes sent so far, throttled against `consumed`.
    let mut sent: u64 = 0;

    let mut sample_buf: Vec<i32> = Vec::new();
    let mut diag_logged = false;

    loop {
        if cancel.load(Relaxed) {
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

        // Apply the pending seek in place (no re-probe), mirroring do_decode_seek.
        // Start from a seeded initial seek, then let a newer live seek win. On
        // success the render flushes between old and new PCM (channel order is the
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
            crate::vprintln!("[WASAPI] live seek to {t:.1}s is out of range");
            pending_seek = None;
            was_initial_seek = false;
            if cmd_tx
                .send(ExclusiveCommand::SeekFailed { stream_id, gen_id })
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
                    crate::vprintln!("[WASAPI] live seek to {t:.1}s (actual {actual:.1}s)");
                    if cmd_tx
                        .send(ExclusiveCommand::ResetForSeek {
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
                    crate::vprintln!("[WASAPI] live seek to {t:.1}s failed: {e}");
                    if was_initial_seek {
                        pending_initial_seek = Some((t, gen_id));
                    } else if cmd_tx
                        .send(ExclusiveCommand::SeekFailed { stream_id, gen_id })
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
                // Real EOF: arm completion, then PARK on the seek channel instead of
                // returning. Dropping seek_rx would kill live seeks for the rest of the
                // track (a cached source decodes far ahead of playback). ResetForSeek
                // un-ends the stream on a later seek, re-arming completion after it.
                if cancel.load(Relaxed) {
                    return Ok(());
                }
                crate::vprintln!("[WASAPI] decode: EOF (stream {stream_id}), parked for seeks");
                let _ = cmd_tx.send(ExclusiveCommand::EndStream { stream_id });
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
                let _ = cmd_tx.send(ExclusiveCommand::DecodeFailed {
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

        sample_buf.clear();
        decoded.copy_to_vec_interleaved::<i32>(&mut sample_buf);

        if !diag_logged && !sample_buf.is_empty() {
            diag_logged = true;
            let mn = sample_buf.iter().copied().min().unwrap_or(0);
            let mx = sample_buf.iter().copied().max().unwrap_or(0);
            crate::vprintln3!(
                "[WASAPI-DIAG] decode src_bps={bits_per_sample} i32 min={mn} max={mx} raw=[{:#010x} {:#010x} {:#010x} {:#010x}]",
                sample_buf.first().copied().unwrap_or(0),
                sample_buf.get(1).copied().unwrap_or(0),
                sample_buf.get(2).copied().unwrap_or(0),
                sample_buf.get(3).copied().unwrap_or(0),
            );
        }

        let mut chunk = Vec::new();
        append_interleaved_i32_as_pcm(&sample_buf, bits_per_sample, &mut chunk);

        let chunk_len = chunk.len() as u64;
        if !chunk.is_empty()
            && cmd_tx
                .send(ExclusiveCommand::PushPcm {
                    stream_id,
                    pcm_data: chunk,
                })
                .is_err()
        {
            return Ok(());
        }
        sent += chunk_len;
    }
}
