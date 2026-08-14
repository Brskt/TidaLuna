use super::output::AudioPipeline;
use super::{DecodeCommand, DecodeEvent};
use crate::player::buffer::RamBuffer;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::mpsc;
use std::time::Duration;
use symphonia::core::codecs::CodecParameters;
use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, SeekMode, SeekTo};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;

pub(super) struct DecodeThreadConfig {
    pub buffer: RamBuffer,
    pub producer: rtrb::Producer<f32>,
    pub decoded_samples: Arc<AtomicU64>,
    pub cmd_rx: mpsc::Receiver<DecodeCommand>,
    pub event_tx: mpsc::Sender<DecodeEvent>,
    pub output_rate: u32,
    pub output_channels: u16,
    pub seek_gen: Arc<AtomicU32>,
}

pub(super) fn spawn_decode_thread(cfg: DecodeThreadConfig) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("decode".into())
        .spawn(move || {
            decode_loop(cfg);
        })
        .expect("failed to spawn decode thread")
}

struct DecodeContext<'a> {
    track_id: u32,
    source_rate: u32,
    output_rate: u32,
    output_channels: u16,
    seek_gen: &'a AtomicU32,
    decoded_samples: &'a AtomicU64,
    event_tx: &'a mpsc::Sender<DecodeEvent>,
}

/// Where the decoder stands right now, from the counter it shares with the ring. Used for
/// the two outcomes that leave it untouched; the landing branch reports the timestamp the
/// reader actually returned, which is exact at the source rate rather than the output one.
fn undisturbed_position_secs(ctx: &DecodeContext) -> f64 {
    let frames = ctx.decoded_samples.load(Relaxed) / ctx.output_channels.max(1) as u64;
    frames as f64 / ctx.output_rate.max(1) as f64
}

fn do_decode_seek(
    time: f64,
    gen_id: u32,
    format: &mut dyn symphonia::core::formats::FormatReader,
    decoder: &mut dyn AudioDecoder,
    pipeline: &mut Option<AudioPipeline>,
    ctx: &DecodeContext,
) {
    let seek_start = std::time::Instant::now();
    let Some(time_pos) = symphonia::core::units::Time::try_from_secs_f64(time) else {
        crate::vprintln!("[SEEK]   invalid seek target: {time}");
        let _ = ctx.event_tx.send(DecodeEvent::SeekComplete {
            gen_id,
            position: undisturbed_position_secs(ctx),
            refused: true,
        });
        return;
    };
    let seek_to = SeekTo::Time {
        time: time_pos,
        track_id: Some(ctx.track_id),
    };
    match format.seek(SeekMode::Coarse, seek_to) {
        Ok(seeked) => {
            let seek_dur = seek_start.elapsed();
            decoder.reset();
            if let Some(p) = pipeline {
                p.reset();
            }
            ctx.seek_gen.fetch_add(1, Relaxed);
            let actual_ts = seeked.actual_ts.get() as u64;
            let out_ts = actual_ts * ctx.output_rate as u64 / ctx.source_rate as u64;
            ctx.decoded_samples
                .store(out_ts * ctx.output_channels as u64, Relaxed);
            let _ = ctx.event_tx.send(DecodeEvent::SeekComplete {
                gen_id,
                position: actual_ts as f64 / ctx.source_rate.max(1) as f64,
                refused: false,
            });
            let seek_ms = seek_dur.as_secs_f64() * 1000.0;
            if seek_ms >= 1.0 {
                crate::vprintln!("[SEEK]   decode: {:.0}ms (ts: {})", seek_ms, actual_ts);
            } else {
                crate::vprintln!(
                    "[SEEK]   decode: {:.0}µs (ts: {})",
                    seek_dur.as_micros(),
                    actual_ts
                );
            }
        }
        Err(e) => {
            let _ = ctx.event_tx.send(DecodeEvent::SeekComplete {
                gen_id,
                position: undisturbed_position_secs(ctx),
                refused: true,
            });
            crate::vprintln!("[SEEK]   symphonia seek failed: {e}");
        }
    }
}

fn decode_loop(cfg: DecodeThreadConfig) {
    let DecodeThreadConfig {
        buffer,
        mut producer,
        decoded_samples,
        cmd_rx,
        event_tx,
        output_rate,
        output_channels,
        seek_gen,
    } = cfg;
    crate::vprintln!("[DECODE] Thread started, probing format...");
    let mss = MediaSourceStream::new(Box::new(buffer), Default::default());

    let hint = Hint::new();
    let format_opts = FormatOptions::default();
    let metadata_opts = MetadataOptions::default();
    let decoder_opts = AudioDecoderOptions::default();

    let mut format =
        match symphonia::default::get_probe().probe(&hint, mss, format_opts, metadata_opts) {
            Ok(f) => f,
            Err(e) => {
                let _ = event_tx.send(DecodeEvent::Error(format!("probe failed: {e}")));
                return;
            }
        };

    let track = match format
        .tracks()
        .iter()
        .find(|t| matches!(&t.codec_params, Some(CodecParameters::Audio(_))))
    {
        Some(t) => t.clone(),
        None => {
            let _ = event_tx.send(DecodeEvent::Error("no audio track found".into()));
            return;
        }
    };

    let track_id = track.id;
    let audio_params = match &track.codec_params {
        Some(CodecParameters::Audio(p)) => p,
        _ => unreachable!("track was selected as audio"),
    };
    let codec_id = audio_params.codec;
    let source_rate = audio_params.sample_rate.unwrap_or(44100);
    let source_channels = audio_params
        .channels
        .as_ref()
        .map(|c| c.count())
        .unwrap_or(2);

    let mut decoder =
        match symphonia::default::get_codecs().make_audio_decoder(audio_params, &decoder_opts) {
            Ok(d) => d,
            Err(e) => {
                let _ = event_tx.send(DecodeEvent::Error(format!("codec init failed: {e}")));
                return;
            }
        };

    let mut pipeline = if source_rate != output_rate || source_channels != output_channels as usize
    {
        crate::vprintln!(
            "[DECODE] Resampling: {}Hz/{}ch -> {}Hz/{}ch",
            source_rate,
            source_channels,
            output_rate,
            output_channels
        );
        Some(AudioPipeline::new(
            source_rate,
            output_rate,
            source_channels,
            output_channels as usize,
        ))
    } else {
        None
    };

    crate::vprintln!(
        "[DECODE] Probe OK: {} {}Hz/{}ch | output: {}Hz/{}ch",
        super::output::codec_name(codec_id),
        source_rate,
        source_channels,
        output_rate,
        output_channels
    );

    let decode_ctx = DecodeContext {
        track_id,
        source_rate,
        output_rate,
        output_channels,
        seek_gen: &seek_gen,
        decoded_samples: &decoded_samples,
        event_tx: &event_tx,
    };

    let mut sample_vec: Vec<f32> = Vec::new();
    let mut paused = true;
    let mut first_packet_logged = false;
    let mut first_push_logged = false;

    loop {
        // Process commands - block when paused (zero CPU), poll when active.
        loop {
            let cmd = if paused {
                match cmd_rx.recv() {
                    Ok(cmd) => cmd,
                    Err(_) => return,
                }
            } else {
                match cmd_rx.try_recv() {
                    Ok(cmd) => cmd,
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => return,
                }
            };
            match cmd {
                DecodeCommand::Seek(time, gen_id) => {
                    do_decode_seek(
                        time,
                        gen_id,
                        &mut *format,
                        &mut *decoder,
                        &mut pipeline,
                        &decode_ctx,
                    );
                }
                DecodeCommand::Pause => {
                    paused = true;
                }
                DecodeCommand::Resume => {
                    crate::vprintln!("[DECODE] Resumed");
                    paused = false;
                }
                DecodeCommand::Stop => {
                    crate::vprintln!("[DECODE] Stop received, exiting");
                    return;
                }
            }
        }

        let packet = match format.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) => {
                // End of stream - flush resampler pipeline before signaling completion
                if let Some(ref mut pipe) = pipeline {
                    let flushed = pipe.flush();
                    let mut off = 0;
                    while off < flushed.len() {
                        let avail = producer.slots();
                        if avail == 0 {
                            std::thread::sleep(Duration::from_millis(1));
                            continue;
                        }
                        let n = (flushed.len() - off).min(avail);
                        if let Ok(chunk) = producer.write_chunk_uninit(n) {
                            off += chunk.fill_from_iter(flushed[off..off + n].iter().copied());
                        }
                    }
                }
                let _ = event_tx.send(DecodeEvent::Finished);
                crate::vprintln!("[DECODE] EOF, parked for seeks");
                // Park rather than return: the reader can still move back before EOF.
                // Returning drops cmd_rx, and a later seek then reaches a dead channel that
                // answers no SeekComplete. A Stop or a dropped sender is the only way out.
                loop {
                    match cmd_rx.recv() {
                        Ok(DecodeCommand::Seek(time, gen_id)) => {
                            do_decode_seek(
                                time,
                                gen_id,
                                &mut *format,
                                &mut *decoder,
                                &mut pipeline,
                                &decode_ctx,
                            );
                            break;
                        }
                        Ok(DecodeCommand::Pause) => paused = true,
                        Ok(DecodeCommand::Resume) => paused = false,
                        Ok(DecodeCommand::Stop) => {
                            crate::vprintln!("[DECODE] Stop received while parked, exiting");
                            return;
                        }
                        Err(_) => return,
                    }
                }
                continue;
            }
            Err(symphonia::core::errors::Error::ResetRequired) => {
                decoder.reset();
                continue;
            }
            Err(e) => {
                let _ = event_tx.send(DecodeEvent::Error(format!("packet error: {e}")));
                let _ = event_tx.send(DecodeEvent::Finished);
                return;
            }
        };

        if packet.track_id != track_id {
            continue;
        }

        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(symphonia::core::errors::Error::DecodeError(e)) => {
                crate::vprintln!("[DECODE] decode error (skipping): {e}");
                continue;
            }
            Err(e) => {
                let _ = event_tx.send(DecodeEvent::Error(format!("decode fatal: {e}")));
                let _ = event_tx.send(DecodeEvent::Finished);
                return;
            }
        };

        let num_frames = decoded.frames();

        sample_vec.clear();
        decoded.copy_to_vec_interleaved::<f32>(&mut sample_vec);
        let source_samples = sample_vec.as_slice();

        if !first_packet_logged {
            first_packet_logged = true;
            crate::vprintln!(
                "[DECODE] First packet: {} frames, {} source samples",
                num_frames,
                source_samples.len()
            );
        }

        let resampled: Vec<f32>;
        let samples_to_push: &[f32] = if let Some(ref mut pipe) = pipeline {
            resampled = pipe.process(source_samples);
            if !first_push_logged && !resampled.is_empty() {
                let (min, max) = resampled
                    .iter()
                    .fold((f32::MAX, f32::MIN), |(mn, mx), &s| (mn.min(s), mx.max(s)));
                crate::vprintln!(
                    "[DECODE] First resampled: {} samples | min={:.6} max={:.6}",
                    resampled.len(),
                    min,
                    max
                );
            }
            &resampled
        } else {
            if !first_push_logged && !source_samples.is_empty() {
                let (min, max) = source_samples
                    .iter()
                    .fold((f32::MAX, f32::MIN), |(mn, mx), &s| (mn.min(s), mx.max(s)));
                crate::vprintln!(
                    "[DECODE] First output (passthrough): {} samples | min={:.6} max={:.6}",
                    source_samples.len(),
                    min,
                    max
                );
            }
            source_samples
        };

        // Push samples to ring buffer, blocking if full
        let mut offset = 0;
        while offset < samples_to_push.len() {
            // Check for stop command during push
            if let Ok(cmd) = cmd_rx.try_recv() {
                match cmd {
                    DecodeCommand::Stop => return,
                    DecodeCommand::Pause => {
                        paused = true;
                        break;
                    }
                    DecodeCommand::Seek(time, gen_id) => {
                        do_decode_seek(
                            time,
                            gen_id,
                            &mut *format,
                            &mut *decoder,
                            &mut pipeline,
                            &decode_ctx,
                        );
                        break;
                    }
                    DecodeCommand::Resume => {
                        paused = false;
                    }
                }
            }

            let available = producer.slots();
            if available == 0 {
                std::thread::sleep(Duration::from_millis(1));
                continue;
            }

            let to_write = (samples_to_push.len() - offset).min(available);
            if let Ok(chunk) = producer.write_chunk_uninit(to_write) {
                let written = chunk
                    .fill_from_iter(samples_to_push[offset..offset + to_write].iter().copied());
                offset += written;
                decoded_samples.fetch_add(to_write as u64, Relaxed);
                if !first_push_logged {
                    first_push_logged = true;
                    crate::vprintln!("[DECODE] First push to ring buffer: {} samples", to_write);
                }
            }
        }
    }
}
