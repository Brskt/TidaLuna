use super::output::AudioPipeline;
use super::{DecodeCommand, DecodeEvent};
use crate::player::buffer::RamBuffer;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::mpsc;
use std::time::Duration;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{CODEC_TYPE_NULL, DecoderOptions};
use symphonia::core::formats::{FormatOptions, SeekMode, SeekTo};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

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

fn do_decode_seek(
    time: f64,
    format: &mut dyn symphonia::core::formats::FormatReader,
    decoder: &mut dyn symphonia::core::codecs::Decoder,
    pipeline: &mut Option<AudioPipeline>,
    ctx: &DecodeContext,
) {
    let seek_start = std::time::Instant::now();
    let seek_to = SeekTo::Time {
        time: symphonia::core::units::Time {
            seconds: time as u64,
            frac: time.fract(),
        },
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
            let out_ts = seeked.actual_ts * ctx.output_rate as u64 / ctx.source_rate as u64;
            ctx.decoded_samples
                .store(out_ts * ctx.output_channels as u64, Relaxed);
            let _ = ctx.event_tx.send(DecodeEvent::SeekComplete);
            let seek_ms = seek_dur.as_secs_f64() * 1000.0;
            if seek_ms >= 1.0 {
                crate::vprintln!(
                    "[SEEK]   decode: {:.0}ms (ts: {})",
                    seek_ms,
                    seeked.actual_ts
                );
            } else {
                crate::vprintln!(
                    "[SEEK]   decode: {:.0}µs (ts: {})",
                    seek_dur.as_micros(),
                    seeked.actual_ts
                );
            }
        }
        Err(e) => {
            let _ = ctx.event_tx.send(DecodeEvent::SeekComplete);
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
    let decoder_opts = DecoderOptions::default();

    let probed =
        match symphonia::default::get_probe().format(&hint, mss, &format_opts, &metadata_opts) {
            Ok(p) => p,
            Err(e) => {
                let _ = event_tx.send(DecodeEvent::Error(format!("probe failed: {e}")));
                return;
            }
        };

    let mut format = probed.format;

    let track = match format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
    {
        Some(t) => t.clone(),
        None => {
            let _ = event_tx.send(DecodeEvent::Error("no audio track found".into()));
            return;
        }
    };

    let track_id = track.id;
    let source_rate = track.codec_params.sample_rate.unwrap_or(44100);
    let source_channels = track.codec_params.channels.map(|c| c.count()).unwrap_or(2);

    let mut decoder =
        match symphonia::default::get_codecs().make(&track.codec_params, &decoder_opts) {
            Ok(d) => d,
            Err(e) => {
                let _ = event_tx.send(DecodeEvent::Error(format!("codec init failed: {e}")));
                return;
            }
        };

    let mut pipeline = if source_rate != output_rate || source_channels != output_channels as usize
    {
        crate::vprintln!(
            "[DECODE] Resampling: {}Hz/{}ch → {}Hz/{}ch",
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
        super::output::codec_name(track.codec_params.codec),
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

    let mut sample_buf: Option<SampleBuffer<f32>> = None;
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
                DecodeCommand::Seek(time) => {
                    do_decode_seek(
                        time,
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
            Ok(p) => p,
            Err(symphonia::core::errors::Error::IoError(ref e))
                if e.kind() == std::io::ErrorKind::UnexpectedEof
                    || e.kind() == std::io::ErrorKind::Interrupted =>
            {
                // EOF - flush resampler pipeline before signaling completion
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
                return;
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

        if packet.track_id() != track_id {
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

        let spec = *decoded.spec();
        let num_frames = decoded.frames();

        // Initialize sample buffer with decoder's max capacity to avoid reallocations
        let sbuf =
            sample_buf.get_or_insert_with(|| SampleBuffer::new(decoded.capacity() as u64, spec));

        sbuf.copy_interleaved_ref(decoded);

        let source_samples = sbuf.samples();

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
                    DecodeCommand::Seek(time) => {
                        do_decode_seek(
                            time,
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
