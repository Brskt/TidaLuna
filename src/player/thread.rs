use super::buffer::RamBuffer;
use super::resume::{RESUME_MIN_SECONDS, ResumeStore};
use super::{LOAD_SEQ, PlayerCommand, PlayerEvent, ResumePolicy, format_ms};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::mpsc;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use rubato::Resampler;
#[cfg(target_os = "windows")]
use std::thread;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{CODEC_TYPE_NULL, DecoderOptions};
use symphonia::core::formats::{FormatOptions, SeekMode, SeekTo};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

#[cfg(target_os = "windows")]
use super::{EXCLUSIVE_STREAM_SEQ, wasapi};
#[cfg(target_os = "windows")]
use wasapi::{ExclusiveCommand, ExclusiveEvent, ExclusiveHandle};

/// Maximum difference (seconds) between a pre-seeked position and a seek
/// target for the pre-seek to be considered "close enough" to skip.
const PRE_SEEK_TOLERANCE: f64 = 2.0;

// ---------------------------------------------------------------------------
// Decode thread communication
// ---------------------------------------------------------------------------

enum DecodeCommand {
    Seek(f64),
    Pause,
    Resume,
    Stop,
}

enum DecodeEvent {
    /// Track decoded to completion (EOF).
    Finished,
    /// Decode error (non-fatal, logged).
    Error(String),
    /// Seek completed — audio output can resume.
    SeekComplete,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn format_sample_rate(rate: u32) -> String {
    if rate.is_multiple_of(1000) {
        format!("{} kHz", rate / 1000)
    } else {
        format!("{:.1} kHz", rate as f64 / 1000.0)
    }
}

fn format_duration_mmss(secs: f64) -> String {
    let total = secs as u32;
    format!("{}:{:02}", total / 60, total % 60)
}

fn codec_name(codec: symphonia::core::codecs::CodecType) -> &'static str {
    use symphonia::core::codecs;
    match codec {
        codecs::CODEC_TYPE_FLAC => "flac",
        codecs::CODEC_TYPE_AAC => "aac",
        codecs::CODEC_TYPE_MP3 => "mp3",
        codecs::CODEC_TYPE_VORBIS => "vorbis",
        codecs::CODEC_TYPE_OPUS => "opus",
        codecs::CODEC_TYPE_PCM_S16LE
        | codecs::CODEC_TYPE_PCM_S24LE
        | codecs::CODEC_TYPE_PCM_S32LE
        | codecs::CODEC_TYPE_PCM_F32LE => "pcm",
        codecs::CODEC_TYPE_ALAC => "alac",
        _ => "unknown",
    }
}

fn enumerate_audio_devices() -> Vec<super::AudioDevice> {
    let host = cpal::default_host();
    let mut devices = vec![super::AudioDevice {
        controllable_volume: true,
        id: "default".to_string(),
        name: "System Default".to_string(),
        r#type: Some("systemDefault".to_string()),
    }];

    if let Ok(output_devices) = host.output_devices() {
        for device in output_devices {
            if let Ok(name) = device.name() {
                devices.push(super::AudioDevice {
                    controllable_volume: true,
                    id: name.clone(),
                    name,
                    r#type: None,
                });
            }
        }
    }

    devices
}

fn find_output_device(device_id: &str) -> Option<cpal::Device> {
    let host = cpal::default_host();
    if device_id == "default" {
        return host.default_output_device();
    }

    host.output_devices()
        .ok()?
        .find(|d| d.name().ok().map(|name| name == device_id).unwrap_or(false))
}

// ---------------------------------------------------------------------------
// cpal stream opening (try source rate, fallback to device default)
// ---------------------------------------------------------------------------

struct OpenedStream {
    stream: cpal::Stream,
    producer: rtrb::Producer<f32>,
    rate: u32,
    channels: u16,
    /// Incremented by the decode thread after a seek. The cpal callback compares
    /// its local copy and drains stale samples when it detects a mismatch.
    seek_gen: Arc<AtomicU32>,
    /// When true, the cpal callback outputs silence and drains the ring buffer.
    /// Set by handle_seek (instant mute), cleared by SeekComplete handler.
    muted: Arc<AtomicBool>,
    /// Set by the cpal error callback: 0 = none, 1 = device disconnected, 2 = unknown error.
    stream_error: Arc<AtomicU8>,
}

/// Build the cpal output callback. Shared between both attempts.
fn build_cpal_callback(
    mut consumer: rtrb::Consumer<f32>,
    volume: Arc<AtomicU32>,
    seek_gen: Arc<AtomicU32>,
    muted: Arc<AtomicBool>,
) -> impl FnMut(&mut [f32], &cpal::OutputCallbackInfo) + Send + 'static {
    let mut local_gen: u32 = 0;
    move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
        let c = &mut consumer;

        // Muted during seek — output silence, drain stale data
        if muted.load(Relaxed) {
            while c.pop().is_ok() {}
            for s in data.iter_mut() {
                *s = 0.0;
            }
            return;
        }

        // Seek gen changed — drain stale samples from before the seek
        let cur_gen = seek_gen.load(Relaxed);
        if cur_gen != local_gen {
            while c.pop().is_ok() {}
            local_gen = cur_gen;
        }

        let v = f32::from_bits(volume.load(Relaxed));
        for sample in data.iter_mut() {
            *sample = if let Ok(s) = c.pop() { s * v } else { 0.0 };
        }
    }
}

/// Try to open a cpal output stream at the source sample rate first (letting the
/// OS handle any resampling). If the device rejects the source rate, fall back
/// to the device's default output config — the caller will then set up a rubato
/// resampling pipeline.
fn open_output_stream(
    device: &cpal::Device,
    source_rate: u32,
    source_channels: u16,
    volume: &Arc<AtomicU32>,
) -> Option<OpenedStream> {
    let seek_gen = Arc::new(AtomicU32::new(0));
    let muted = Arc::new(AtomicBool::new(false));
    let stream_error = Arc::new(AtomicU8::new(0));

    let dev_name = device.name().unwrap_or_else(|_| "<unknown>".to_string());
    crate::vprintln!("[CPAL]   Device: {}", dev_name);

    // Attempt 1: source rate (no software resampling needed)
    {
        let config = cpal::StreamConfig {
            channels: source_channels,
            sample_rate: cpal::SampleRate(source_rate),
            buffer_size: cpal::BufferSize::Default,
        };
        let ring_size = source_rate as usize * source_channels as usize * 2;
        let (producer, consumer) = rtrb::RingBuffer::new(ring_size);

        let cb = build_cpal_callback(consumer, volume.clone(), seek_gen.clone(), muted.clone());
        let err_flag = stream_error.clone();
        if let Ok(stream) = device.build_output_stream(
            &config,
            cb,
            move |err| {
                eprintln!("[CPAL]   Stream error: {err}");
                let code = match err {
                    cpal::StreamError::DeviceNotAvailable => 1,
                    _ => 2,
                };
                err_flag.store(code, Relaxed);
            },
            None,
        ) {
            crate::vprintln!(
                "[CPAL]   Opened at source rate: {}Hz/{}ch",
                source_rate,
                source_channels
            );
            return Some(OpenedStream {
                stream,
                producer,
                rate: source_rate,
                channels: source_channels,
                seek_gen,
                muted,
                stream_error,
            });
        }
    }

    // Attempt 2: device default (will need rubato resampling)
    let default = device.default_output_config().ok()?;
    let ar = default.sample_rate().0;
    let ac = default.channels();
    let cfg = default.config();

    crate::vprintln!(
        "[CPAL]   Source rate {}Hz unsupported, using device default: {}Hz/{}ch",
        source_rate,
        ar,
        ac
    );

    let ring_size = ar as usize * ac as usize * 2;
    let (producer, consumer) = rtrb::RingBuffer::new(ring_size);

    let cb = build_cpal_callback(consumer, volume.clone(), seek_gen.clone(), muted.clone());
    let err_flag = stream_error.clone();
    match device.build_output_stream(
        &cfg,
        cb,
        move |err| {
            eprintln!("[CPAL]   Stream error: {err}");
            let code = match err {
                cpal::StreamError::DeviceNotAvailable => 1,
                _ => 2,
            };
            err_flag.store(code, Relaxed);
        },
        None,
    ) {
        Ok(stream) => Some(OpenedStream {
            stream,
            producer,
            rate: ar,
            channels: ac,
            seek_gen,
            muted,
            stream_error,
        }),
        Err(e) => {
            eprintln!("[ERROR]  Failed to open cpal stream: {e}");
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Audio resampling pipeline (rubato)
// ---------------------------------------------------------------------------

/// Handles sample-rate conversion and channel remapping between the source
/// format (from symphonia) and the output device format (from cpal).
struct AudioPipeline {
    resampler: rubato::SincFixedIn<f32>,
    source_channels: usize,
    output_channels: usize,
    chunk_size: usize,
    accum: Vec<Vec<f32>>,
    accum_frames: usize,
}

impl AudioPipeline {
    fn new(
        source_rate: u32,
        output_rate: u32,
        source_channels: usize,
        output_channels: usize,
    ) -> Self {
        let ratio = output_rate as f64 / source_rate as f64;
        let chunk_size = 1024;
        let params = rubato::SincInterpolationParameters {
            sinc_len: 256,
            f_cutoff: 0.95,
            interpolation: rubato::SincInterpolationType::Linear,
            oversampling_factor: 256,
            window: rubato::WindowFunction::BlackmanHarris2,
        };

        let resampler =
            rubato::SincFixedIn::<f32>::new(ratio, 2.0, params, chunk_size, source_channels)
                .expect("failed to create resampler");

        Self {
            resampler,
            source_channels,
            output_channels,
            chunk_size,
            accum: vec![Vec::with_capacity(chunk_size * 2); source_channels],
            accum_frames: 0,
        }
    }

    /// Feed interleaved source samples, returns interleaved output samples
    /// in the output channel layout.
    fn process(&mut self, interleaved: &[f32]) -> Vec<f32> {
        let src_ch = self.source_channels;
        let frames = interleaved.len() / src_ch;

        // De-interleave into per-channel accumulation buffers
        for f in 0..frames {
            for ch in 0..src_ch {
                self.accum[ch].push(interleaved[f * src_ch + ch]);
            }
        }
        self.accum_frames += frames;

        let mut output = Vec::new();

        // Process full chunks through resampler
        while self.accum_frames >= self.chunk_size {
            let input: Vec<&[f32]> = self.accum.iter().map(|ch| &ch[..self.chunk_size]).collect();

            match self.resampler.process(&input, None) {
                Ok(resampled) => {
                    let out_frames = resampled.first().map(|c| c.len()).unwrap_or(0);
                    self.interleave_into(&resampled, out_frames, &mut output);
                }
                Err(e) => {
                    eprintln!("[RESAMPLE] Error: {e}");
                }
            }

            for ch in &mut self.accum {
                ch.drain(..self.chunk_size);
            }
            self.accum_frames -= self.chunk_size;
        }

        output
    }

    /// Flush remaining accumulated samples (call at EOF).
    fn flush(&mut self) -> Vec<f32> {
        if self.accum_frames == 0 {
            return Vec::new();
        }

        let input: Vec<&[f32]> = self.accum.iter().map(|ch| &ch[..]).collect();
        let mut output = Vec::new();

        match self.resampler.process_partial(Some(&input), None) {
            Ok(resampled) => {
                let out_frames = resampled.first().map(|c| c.len()).unwrap_or(0);
                self.interleave_into(&resampled, out_frames, &mut output);
            }
            Err(e) => {
                eprintln!("[RESAMPLE] Flush error: {e}");
            }
        }

        for ch in &mut self.accum {
            ch.clear();
        }
        self.accum_frames = 0;
        output
    }

    fn reset(&mut self) {
        for ch in &mut self.accum {
            ch.clear();
        }
        self.accum_frames = 0;
        self.resampler.reset();
    }

    #[allow(clippy::needless_range_loop)]
    fn interleave_into(&self, resampled: &[Vec<f32>], frames: usize, output: &mut Vec<f32>) {
        let src_ch = self.source_channels;
        let out_ch = self.output_channels;
        output.reserve(frames * out_ch);

        for f_idx in 0..frames {
            for ch in 0..out_ch {
                let sample = if ch < src_ch {
                    resampled[ch][f_idx]
                } else if src_ch == 1 {
                    resampled[0][f_idx] // mono → multi: duplicate
                } else {
                    0.0 // extra channels: silence
                };
                output.push(sample);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Decode thread
// ---------------------------------------------------------------------------

/// Spawn the decode thread. Returns command sender, event receiver, and join handle.
#[allow(clippy::too_many_arguments)]
fn spawn_decode_thread(
    buffer: RamBuffer,
    producer: rtrb::Producer<f32>,
    decoded_samples: Arc<AtomicU64>,
    cmd_rx: mpsc::Receiver<DecodeCommand>,
    event_tx: mpsc::Sender<DecodeEvent>,
    output_rate: u32,
    output_channels: u16,
    seek_gen: Arc<AtomicU32>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("decode".into())
        .spawn(move || {
            decode_loop(
                buffer,
                producer,
                decoded_samples,
                cmd_rx,
                event_tx,
                output_rate,
                output_channels,
                seek_gen,
            );
        })
        .expect("failed to spawn decode thread")
}

/// Execute a symphonia seek, reset decoder/resampler, update decoded_samples
/// counter, signal SeekComplete, and log timing.
#[allow(clippy::too_many_arguments)]
fn do_decode_seek(
    time: f64,
    format: &mut dyn symphonia::core::formats::FormatReader,
    decoder: &mut dyn symphonia::core::codecs::Decoder,
    pipeline: &mut Option<AudioPipeline>,
    track_id: u32,
    source_rate: u32,
    output_rate: u32,
    output_channels: u16,
    seek_gen: &AtomicU32,
    decoded_samples: &AtomicU64,
    event_tx: &mpsc::Sender<DecodeEvent>,
) {
    let seek_start = std::time::Instant::now();
    let seek_to = SeekTo::Time {
        time: symphonia::core::units::Time {
            seconds: time as u64,
            frac: time.fract(),
        },
        track_id: Some(track_id),
    };
    match format.seek(SeekMode::Coarse, seek_to) {
        Ok(seeked) => {
            let seek_dur = seek_start.elapsed();
            decoder.reset();
            if let Some(p) = pipeline {
                p.reset();
            }
            seek_gen.fetch_add(1, Relaxed);
            let out_ts = seeked.actual_ts * output_rate as u64 / source_rate as u64;
            decoded_samples.store(out_ts * output_channels as u64, Relaxed);
            let _ = event_tx.send(DecodeEvent::SeekComplete);
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
            let _ = event_tx.send(DecodeEvent::SeekComplete);
            crate::vprintln!("[SEEK]   symphonia seek failed: {e}");
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn decode_loop(
    buffer: RamBuffer,
    mut producer: rtrb::Producer<f32>,
    decoded_samples: Arc<AtomicU64>,
    cmd_rx: mpsc::Receiver<DecodeCommand>,
    event_tx: mpsc::Sender<DecodeEvent>,
    output_rate: u32,
    output_channels: u16,
    seek_gen: Arc<AtomicU32>,
) {
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

    // Find the first audio track
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

    // Create resampling pipeline if source and output formats differ
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
        codec_name(track.codec_params.codec),
        source_rate,
        source_channels,
        output_rate,
        output_channels
    );

    let mut sample_buf: Option<SampleBuffer<f32>> = None;
    let mut paused = true;
    let mut first_packet_logged = false;
    let mut first_push_logged = false;

    loop {
        // Process commands — block when paused (zero CPU), poll when active.
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
                        track_id,
                        source_rate,
                        output_rate,
                        output_channels,
                        &seek_gen,
                        &decoded_samples,
                        &event_tx,
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

        // Decode next packet
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(symphonia::core::errors::Error::IoError(ref e))
                if e.kind() == std::io::ErrorKind::UnexpectedEof
                    || e.kind() == std::io::ErrorKind::Interrupted =>
            {
                // EOF — flush resampler pipeline before signaling completion
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

        // Skip packets from other tracks
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

        // Process through resampling pipeline if needed
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
                            track_id,
                            source_rate,
                            output_rate,
                            output_channels,
                            &seek_gen,
                            &decoded_samples,
                            &event_tx,
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

// ---------------------------------------------------------------------------
// PlayerThread
// ---------------------------------------------------------------------------

pub(super) struct PlayerThread<F> {
    cmd_rx: mpsc::Receiver<PlayerCommand>,
    callback: F,
    // Audio output
    cpal_stream: Option<cpal::Stream>,
    volume: Arc<AtomicU32>, // f32 bits stored as u32
    // Decode thread
    decode_cmd_tx: Option<mpsc::Sender<DecodeCommand>>,
    decode_event_rx: Option<mpsc::Receiver<DecodeEvent>>,
    decode_handle: Option<std::thread::JoinHandle<()>>,
    // Track state
    current_buffer: Option<RamBuffer>,
    current_track_id: Option<String>,
    is_cached: bool,
    is_playing: bool,
    has_track: bool,
    current_duration: f64,
    current_seq: u32,
    // Position tracking
    decoded_samples: Arc<AtomicU64>,
    sample_rate: u32,
    channels: u16,
    // Resume
    resume_store: ResumeStore,
    pending_resume_seek: Option<f64>,
    /// Pre-seek position sent to the paused decode thread during handle_load.
    /// If a subsequent seek targets the same position, we skip the redundant
    /// network seek.
    pre_seek_pos: Option<f64>,
    allow_startup_auto_resume: bool,
    // Device
    current_device_id: Option<String>,
    // WASAPI exclusive
    #[cfg(target_os = "windows")]
    exclusive_handle: Option<ExclusiveHandle>,
    #[cfg(target_os = "windows")]
    is_exclusive_mode: bool,
    #[cfg(target_os = "windows")]
    exclusive_stream_cancel: Option<Arc<AtomicBool>>,
    // Seek state — suppresses TimeUpdates while decode thread is seeking
    seeking: bool,
    /// Target position during an active seek — emitted as TimeUpdate so the
    /// frontend's seek bar stays pinned at the target instead of freezing.
    seek_target: Option<f64>,
    /// When the current seek started (for total seek timing)
    seek_wall_start: Option<std::time::Instant>,
    /// Shared with cpal callback — instantly mutes output when true
    cpal_muted: Option<Arc<AtomicBool>>,
    /// Shared with cpal error callback: 0 = none, 1 = disconnected, 2 = unknown
    cpal_stream_error: Option<Arc<AtomicU8>>,
    /// Diagnostic: non-zero samples read by cpal callback
    // Buffering state — emits idle/active when decode thread stalls on RamBuffer
    buffer_stalled: bool,
    // Version event fire-once flag
    version_emitted: bool,
    // Command coalescing
    pending_cmds: Vec<PlayerCommand>,
    coalesced_cmds: Vec<PlayerCommand>,
}

impl<F: Fn(PlayerEvent) + Send + 'static> PlayerThread<F> {
    pub fn new(cmd_rx: mpsc::Receiver<PlayerCommand>, callback: F) -> Option<Self> {
        Some(Self {
            cmd_rx,
            callback,
            cpal_stream: None,
            volume: Arc::new(AtomicU32::new(f32::to_bits(1.0))),
            decode_cmd_tx: None,
            decode_event_rx: None,
            decode_handle: None,
            current_buffer: None,
            current_track_id: None,
            is_cached: false,
            is_playing: false,
            has_track: false,
            current_duration: 0.0,
            current_seq: 0,
            decoded_samples: Arc::new(AtomicU64::new(0)),
            sample_rate: 44100,
            channels: 2,
            resume_store: ResumeStore::load(),
            pending_resume_seek: None,
            pre_seek_pos: None,
            allow_startup_auto_resume: true,
            current_device_id: None,
            #[cfg(target_os = "windows")]
            exclusive_handle: None,
            #[cfg(target_os = "windows")]
            is_exclusive_mode: false,
            #[cfg(target_os = "windows")]
            exclusive_stream_cancel: None,
            seeking: false,
            seek_target: None,
            seek_wall_start: None,
            cpal_muted: None,
            cpal_stream_error: None,
            buffer_stalled: false,
            version_emitted: false,
            pending_cmds: Vec::new(),
            coalesced_cmds: Vec::new(),
        })
    }

    pub fn run(&mut self) {
        loop {
            // Drain any commands that arrived during processing
            while let Ok(cmd) = self.cmd_rx.try_recv() {
                self.pending_cmds.push(cmd);
            }

            // Coalesce seek bursts
            self.coalesced_cmds.clear();
            let mut pending_seek: Option<f64> = None;
            for cmd in self.pending_cmds.drain(..) {
                match cmd {
                    PlayerCommand::Seek(time) => {
                        pending_seek = Some(time);
                    }
                    other => {
                        if let Some(time) = pending_seek.take() {
                            self.coalesced_cmds.push(PlayerCommand::Seek(time));
                        }
                        self.coalesced_cmds.push(other);
                    }
                }
            }
            if let Some(time) = pending_seek.take() {
                self.coalesced_cmds.push(PlayerCommand::Seek(time));
            }

            let cmds: Vec<PlayerCommand> = self.coalesced_cmds.drain(..).collect();
            for cmd in cmds {
                self.handle_command(cmd);
            }

            // Poll exclusive WASAPI events
            #[cfg(target_os = "windows")]
            self.poll_exclusive_events();

            // Poll playback state
            self.poll_playback();

            // Wait for next command — short poll while seeking
            let timeout = if self.seeking {
                Duration::from_millis(1)
            } else {
                Duration::from_millis(250)
            };
            if let Ok(cmd) = self.cmd_rx.recv_timeout(timeout) {
                self.pending_cmds.push(cmd);
                while let Ok(cmd) = self.cmd_rx.try_recv() {
                    self.pending_cmds.push(cmd);
                }
            }
        }
    }

    fn handle_command(&mut self, cmd: PlayerCommand) {
        match cmd {
            PlayerCommand::Load {
                buffer,
                load_gen,
                seq,
                track_id,
                resume_policy,
                load_start,
                cached,
            } => {
                self.handle_load(
                    buffer,
                    load_gen,
                    seq,
                    track_id,
                    resume_policy,
                    load_start,
                    cached,
                );
            }
            PlayerCommand::Play => self.handle_play(),
            PlayerCommand::Pause => self.handle_pause(),
            PlayerCommand::Stop(event_seq) => self.handle_stop(event_seq),
            PlayerCommand::Seek(time) => self.handle_seek(time),
            PlayerCommand::SetVolume(vol) => self.handle_set_volume(vol),
            PlayerCommand::GetAudioDevices(req_id) => self.handle_get_audio_devices(req_id),
            PlayerCommand::SetAudioDevice { id, exclusive } => {
                self.handle_set_audio_device(id, exclusive);
            }
            PlayerCommand::EmitMediaError { error, code } => {
                (self.callback)(PlayerEvent::MediaError { error, code });
            }
            PlayerCommand::EmitMaxConnections => {
                (self.callback)(PlayerEvent::MaxConnectionsReached);
            }
        }
    }

    fn resolve_resume_policy(&self, resume_policy: ResumePolicy, track_id: &str) -> Option<f64> {
        match resume_policy {
            ResumePolicy::Disabled => {
                if self.allow_startup_auto_resume {
                    self.resume_store.get(track_id)
                } else {
                    None
                }
            }
            ResumePolicy::Auto => self.resume_store.get(track_id),
            ResumePolicy::Explicit(t) => {
                if t.is_finite() && t > RESUME_MIN_SECONDS {
                    Some(t)
                } else {
                    None
                }
            }
        }
    }

    /// Stop the current decode thread and cpal stream.
    fn stop_decode(&mut self) {
        if let Some(tx) = self.decode_cmd_tx.take() {
            let _ = tx.send(DecodeCommand::Stop);
        }
        // Drop the cpal stream (stops the audio callback)
        self.cpal_stream = None;
        if let Some(handle) = self.decode_handle.take() {
            let _ = handle.join();
        }
        self.decode_event_rx = None;
    }

    #[allow(clippy::too_many_arguments)]
    fn handle_load(
        &mut self,
        buffer: RamBuffer,
        load_gen: u32,
        event_seq: u32,
        track_id: String,
        resume_policy: ResumePolicy,
        load_start: std::time::Instant,
        cached: bool,
    ) {
        if load_gen != LOAD_SEQ.load(Relaxed) {
            crate::vprintln!("[LOAD #{load_gen}] stale Load, ignoring");
            return;
        }

        // Clear previous track's resume position
        if let Some(ref prev) = self.current_track_id
            && *prev != track_id
        {
            self.resume_store.clear(prev);
        }

        self.current_track_id = Some(track_id.clone());
        self.pending_resume_seek = self.resolve_resume_policy(resume_policy, &track_id);
        self.current_seq = event_seq;
        self.is_cached = cached;
        self.buffer_stalled = false;

        crate::vprintln!(
            "[LOAD #{load_gen}] handle_load enter | cached={} | track={}",
            cached,
            track_id.chars().take(60).collect::<String>()
        );
        let handle_start = std::time::Instant::now();

        // Cancel previous playback
        #[cfg(target_os = "windows")]
        if let Some(cancel) = self.exclusive_stream_cancel.take() {
            cancel.store(true, Relaxed);
        }
        if let Some(ref old_buf) = self.current_buffer {
            old_buf.cancel();
        }
        self.stop_decode();

        let teardown_ms = handle_start.elapsed().as_secs_f64() * 1000.0;
        let decode_start = std::time::Instant::now();

        // WASAPI exclusive path
        #[cfg(target_os = "windows")]
        {
            if self.is_exclusive_mode {
                if let Some(ref handle) = self.exclusive_handle {
                    handle.send(ExclusiveCommand::Stop);
                    let cancel = Arc::new(AtomicBool::new(false));
                    self.exclusive_stream_cancel = Some(cancel.clone());

                    let cmd_tx = handle.command_sender();
                    let reader = buffer.clone();
                    let total_len = buffer.total_len();
                    let stream_id = EXCLUSIVE_STREAM_SEQ.fetch_add(1, Relaxed) + 1;
                    thread::spawn(move || {
                        if let Err(e) = wasapi::stream_flac_reader_to_wasapi(
                            reader,
                            total_len,
                            stream_id,
                            cmd_tx,
                            cancel.clone(),
                        ) && !cancel.load(Relaxed)
                        {
                            eprintln!("[WASAPI] Stream decode failed: {e}");
                        }
                    });

                    crate::vprintln!(
                        "[WASAPI] Progressive decode started ({:.0}ms setup)",
                        decode_start.elapsed().as_secs_f64() * 1000.0
                    );
                    self.current_buffer = Some(buffer);
                    self.has_track = true;
                    self.is_playing = true;
                }
                return;
            }
        }

        // Shared mode: symphonia + cpal
        let total_len = buffer.total_len();

        // Probe the format with symphonia
        let probe_reader = buffer.clone();
        let mss = MediaSourceStream::new(Box::new(probe_reader), Default::default());
        let hint = Hint::new();

        let probed = match symphonia::default::get_probe().format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        ) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("[ERROR]  Probe failed: {e}");
                return;
            }
        };

        let probe_ms = decode_start.elapsed().as_secs_f64() * 1000.0;
        crate::vprintln!("[LOAD #{load_gen}] probe: {}", format_ms(probe_ms));

        // Get audio track info
        let track = match probed
            .format
            .tracks()
            .iter()
            .find(|t| t.codec_params.codec != symphonia::core::codecs::CODEC_TYPE_NULL)
        {
            Some(t) => t,
            None => {
                eprintln!("[ERROR]  No audio track found");
                return;
            }
        };

        let source_sample_rate = track.codec_params.sample_rate.unwrap_or(44100);
        let source_channels = track
            .codec_params
            .channels
            .map(|c| c.count() as u16)
            .unwrap_or(2);
        let source_duration = track
            .codec_params
            .n_frames
            .map(|n| n as f64 / source_sample_rate as f64)
            .unwrap_or(0.0);
        let source_bit_depth = track.codec_params.bits_per_sample;
        let source_codec = codec_name(track.codec_params.codec);

        drop(probed); // Release the probe reader

        self.current_duration = source_duration;
        self.decoded_samples.store(0, Relaxed);

        // Emit version once (fire-once at first load)
        if !self.version_emitted {
            self.version_emitted = true;
            (self.callback)(PlayerEvent::Version(env!("CARGO_PKG_VERSION")));
        }

        // Emit media format info to the frontend
        (self.callback)(PlayerEvent::MediaFormat {
            codec: source_codec,
            sample_rate: source_sample_rate,
            bit_depth: source_bit_depth,
            channels: source_channels,
        });

        // Open cpal stream (try source rate first, fall back to device default + rubato)
        let device = if let Some(ref id) = self.current_device_id {
            match find_output_device(id) {
                Some(d) => d,
                None => {
                    crate::vprintln!("[AUDIO] Device '{}' not found, falling back to default", id);
                    (self.callback)(PlayerEvent::DeviceError("devicenotfound"));
                    match cpal::default_host().default_output_device() {
                        Some(d) => d,
                        None => {
                            (self.callback)(PlayerEvent::DeviceError("devicenotfound"));
                            return;
                        }
                    }
                }
            }
        } else {
            match cpal::default_host().default_output_device() {
                Some(d) => d,
                None => {
                    (self.callback)(PlayerEvent::DeviceError("devicenotfound"));
                    return;
                }
            }
        };

        let cpal_start = std::time::Instant::now();
        let opened =
            match open_output_stream(&device, source_sample_rate, source_channels, &self.volume) {
                Some(o) => o,
                None => {
                    (self.callback)(PlayerEvent::DeviceError("deviceformatnotsupported"));
                    return;
                }
            };
        let cpal_ms = cpal_start.elapsed().as_secs_f64() * 1000.0;
        crate::vprintln!("[LOAD #{load_gen}] cpal open: {}", format_ms(cpal_ms));

        let actual_rate = opened.rate;
        let actual_channels = opened.channels;
        let stream = opened.stream;
        let ring_producer = opened.producer;
        let seek_gen = opened.seek_gen;
        self.cpal_muted = Some(opened.muted);
        self.cpal_stream_error = Some(opened.stream_error);

        // Track position at output rate/channels
        self.sample_rate = actual_rate;
        self.channels = actual_channels;

        // Spawn decode thread
        let (decode_cmd_tx, decode_cmd_rx) = mpsc::channel();
        let (decode_event_tx, decode_event_rx) = mpsc::channel();
        let decoded_samples = self.decoded_samples.clone();

        let decode_buffer = buffer.clone();
        let decode_handle = spawn_decode_thread(
            decode_buffer,
            ring_producer,
            decoded_samples,
            decode_cmd_rx,
            decode_event_tx,
            actual_rate,
            actual_channels,
            seek_gen,
        );

        self.cpal_stream = Some(stream);
        self.decode_cmd_tx = Some(decode_cmd_tx);
        self.decode_event_rx = Some(decode_event_rx);
        self.decode_handle = Some(decode_handle);
        self.current_buffer = Some(buffer);
        self.has_track = true;
        self.is_playing = false;

        // Pre-seek: send seek to the paused decode thread now so data starts
        // downloading from near the resume position. When play arrives later,
        // the data is already buffered — no network seek needed.
        self.pre_seek_pos = None;
        if let Some(pos) = self.pending_resume_seek
            && let Some(ref tx) = self.decode_cmd_tx
        {
            let _ = tx.send(DecodeCommand::Seek(pos));
            self.pre_seek_pos = Some(pos);
            crate::vprintln!("[LOAD #{load_gen}] pre-seek to {:.1}s (decode paused)", pos);
        }

        // Emit events
        if self.current_duration > 0.0 {
            (self.callback)(PlayerEvent::Duration(
                self.current_duration,
                self.current_seq,
            ));
        }
        // NOTE: Do NOT emit TimeUpdate(resume_position) here.
        // Sending a non-zero time during load confuses the TIDAL webapp's
        // state machine — it thinks playback is active and the play button
        // becomes a no-op.  The resume position is sent to the frontend
        // only when playback actually starts (deferred play resolution).

        let bitrate = if self.current_duration > 0.0 {
            (total_len as f64 * 8.0 / self.current_duration / 1000.0) as u32
        } else {
            0
        };
        // Publish bitrate (bytes/sec) for governor
        let bitrate_bps = if self.current_duration > 0.0 {
            (total_len as f64 / self.current_duration) as u64
        } else {
            0
        };
        {
            let bp = crate::state::GOVERNOR.buffer_progress();
            bp.bitrate_bps.store(bitrate_bps, Relaxed);
            bp.total_len.store(total_len, Relaxed);
            if let Some(ref buf) = self.current_buffer {
                bp.written.store(buf.written(), Relaxed);
                bp.read_pos.store(buf.read_cursor(), Relaxed);
            }
        }

        crate::vprintln!(
            "[CODEC]  {} / {}ch | {} kbps | {}",
            format_sample_rate(source_sample_rate),
            source_channels,
            bitrate,
            format_duration_mmss(self.current_duration)
        );
        crate::vprintln!(
            "[LOAD #{load_gen}] pipeline: teardown={} probe={} cpal={} total={}{}",
            format_ms(teardown_ms),
            format_ms(probe_ms),
            format_ms(cpal_ms),
            format_ms(handle_start.elapsed().as_secs_f64() * 1000.0),
            if cached {
                " (CACHE HIT)"
            } else {
                " (streaming)"
            }
        );
        crate::vprintln!(
            "[LOAD #{load_gen}] ready in {} (from load_with_policy entry)",
            format_ms(load_start.elapsed().as_secs_f64() * 1000.0)
        );

        (self.callback)(PlayerEvent::StateChange("ready", self.current_seq));
    }

    fn handle_play(&mut self) {
        self.allow_startup_auto_resume = false;

        #[cfg(target_os = "windows")]
        {
            if self.is_exclusive_mode {
                if let Some(seek_time) = self.pending_resume_seek.take()
                    && let Some(ref handle) = self.exclusive_handle
                {
                    handle.send(ExclusiveCommand::Seek(seek_time));
                    (self.callback)(PlayerEvent::TimeUpdate(seek_time, self.current_seq));
                }
                if let Some(ref handle) = self.exclusive_handle {
                    handle.send(ExclusiveCommand::Play);
                }
                self.is_playing = true;
                crate::state::GOVERNOR
                    .buffer_progress()
                    .set_playback_active(true);
                return;
            }
        }

        self.is_playing = true;
        crate::state::GOVERNOR
            .buffer_progress()
            .set_playback_active(true);
        (self.callback)(PlayerEvent::StateChange("active", self.current_seq));

        // If there's a pending resume position, the decode thread was already
        // pre-seeked to it during handle_load(). Emit TimeUpdate so the frontend
        // knows the position, then start playback immediately (no timeout).
        if let Some(pos) = self.pending_resume_seek.take() {
            (self.callback)(PlayerEvent::TimeUpdate(pos.max(0.0), self.current_seq));
            crate::vprintln!("[PLAY]   start at resume {:.1}s (pre-seeked)", pos);
        } else {
            crate::vprintln!("[PLAY]   start from beginning");
        }
        self.start_playback();
    }

    /// Actually start cpal stream + decode. Called after resume seek or deferred play resolves.
    fn start_playback(&mut self) {
        if let Some(ref stream) = self.cpal_stream {
            match stream.play() {
                Ok(()) => crate::vprintln!("[PLAY]   cpal stream.play() OK"),
                Err(e) => eprintln!("[ERROR]  cpal stream.play() failed: {e}"),
            }
        } else {
            eprintln!("[ERROR]  start_playback: no cpal stream!");
        }
        if let Some(ref tx) = self.decode_cmd_tx {
            let _ = tx.send(DecodeCommand::Resume);
            crate::vprintln!("[PLAY]   DecodeCommand::Resume sent");
        } else {
            eprintln!("[ERROR]  start_playback: no decode_cmd_tx!");
        }
        // Pre-seek has served its purpose once playback starts.
        self.pre_seek_pos = None;
    }

    /// If `pre_seek_pos` matches `target` within [`PRE_SEEK_TOLERANCE`], emit
    /// a `TimeUpdate` and return `true` (caller should skip the seek).
    /// Always consumes `pre_seek_pos` regardless of match.
    fn try_skip_pre_seek(&mut self, target: f64) -> bool {
        if let Some(pre_pos) = self.pre_seek_pos.take()
            && (pre_pos - target).abs() < PRE_SEEK_TOLERANCE
        {
            (self.callback)(PlayerEvent::TimeUpdate(target.max(0.0), self.current_seq));
            return true;
        }
        false
    }

    fn handle_pause(&mut self) {
        #[cfg(target_os = "windows")]
        {
            if self.is_exclusive_mode {
                if let Some(ref handle) = self.exclusive_handle {
                    handle.send(ExclusiveCommand::Pause);
                }
                self.is_playing = false;
                crate::state::GOVERNOR
                    .buffer_progress()
                    .set_playback_active(false);
                self.resume_store.flush_if_due(true);
                return;
            }
        }

        if let Some(ref stream) = self.cpal_stream {
            let _ = stream.pause();
        }
        if let Some(ref tx) = self.decode_cmd_tx {
            let _ = tx.send(DecodeCommand::Pause);
        }

        // Emit current position
        let pos_secs = self.current_position_secs();
        (self.callback)(PlayerEvent::TimeUpdate(pos_secs, self.current_seq));

        self.is_playing = false;
        crate::state::GOVERNOR
            .buffer_progress()
            .set_playback_active(false);
        (self.callback)(PlayerEvent::StateChange("paused", self.current_seq));
        self.resume_store.flush_if_due(true);
    }

    fn handle_stop(&mut self, event_seq: u32) {
        self.current_seq = event_seq;
        crate::state::GOVERNOR.reset_buffer_progress();

        #[cfg(target_os = "windows")]
        if let Some(cancel) = self.exclusive_stream_cancel.take() {
            cancel.store(true, Relaxed);
        }

        if let Some(ref old_buf) = self.current_buffer {
            old_buf.cancel();
        }
        self.stop_decode();
        self.current_buffer = None;

        #[cfg(target_os = "windows")]
        {
            if self.is_exclusive_mode {
                if let Some(ref handle) = self.exclusive_handle {
                    handle.send(ExclusiveCommand::Stop);
                }
                (self.callback)(PlayerEvent::TimeUpdate(0.0, self.current_seq));
                (self.callback)(PlayerEvent::StateChange("stopped", self.current_seq));
                self.is_playing = false;
                self.has_track = false;
                self.current_duration = 0.0;
                self.current_track_id = None;
                self.pending_resume_seek = None;
                self.pre_seek_pos = None;
                self.resume_store.flush_if_due(true);
                return;
            }
        }

        (self.callback)(PlayerEvent::TimeUpdate(0.0, self.current_seq));
        (self.callback)(PlayerEvent::StateChange("stopped", self.current_seq));
        self.is_playing = false;
        self.has_track = false;
        self.current_duration = 0.0;
        self.current_track_id = None;
        self.pending_resume_seek = None;
        self.pre_seek_pos = None;
        self.is_cached = false;
        self.buffer_stalled = false;
        self.resume_store.flush_if_due(true);
    }

    fn handle_seek(&mut self, time: f64) {
        // Latest-seek-wins
        let mut latest_time = time;
        while let Ok(next_cmd) = self.cmd_rx.try_recv() {
            match next_cmd {
                PlayerCommand::Seek(t) => {
                    latest_time = t;
                }
                other => self.pending_cmds.push(other),
            }
        }

        if let Some(track_id) = self.current_track_id.as_ref() {
            self.resume_store.set(track_id, latest_time);
            self.resume_store.flush_if_due(false);
        }
        self.pending_resume_seek = None;

        #[cfg(target_os = "windows")]
        {
            if self.is_exclusive_mode {
                if self.has_track {
                    if let Some(ref handle) = self.exclusive_handle {
                        handle.send(ExclusiveCommand::Seek(latest_time));
                    }
                    (self.callback)(PlayerEvent::TimeUpdate(
                        latest_time.max(0.0),
                        self.current_seq,
                    ));
                } else {
                    self.pending_resume_seek = Some(latest_time);
                }
                return;
            }
        }

        if self.try_skip_pre_seek(latest_time) {
            crate::vprintln!("[SEEK]   skipped (pre-seeked matches {:.2}s)", latest_time);
            return;
        }

        if let Some(ref tx) = self.decode_cmd_tx {
            // Mute output instantly + pin TimeUpdates at target until SeekComplete
            self.seeking = true;
            self.seek_target = Some(latest_time);
            self.seek_wall_start = Some(std::time::Instant::now());
            // Pause preload to free HTTP/2 bandwidth for Range restarts
            crate::state::GOVERNOR
                .buffer_progress()
                .request_seek_preload_pause();
            if let Some(ref m) = self.cpal_muted {
                m.store(true, Relaxed);
            }
            // Notify the SDK/webapp that we're seeking — nativePlayer.ts maps
            // "seeking" to STALLED internally.
            (self.callback)(PlayerEvent::StateChange("seeking", self.current_seq));
            (self.callback)(PlayerEvent::TimeUpdate(
                latest_time.max(0.0),
                self.current_seq,
            ));

            let _ = tx.send(DecodeCommand::Seek(latest_time));
            crate::vprintln!(
                "[SEEK]   sent: {:.2}s ({})",
                latest_time,
                if self.is_cached {
                    "cached/RAM"
                } else {
                    "streaming"
                }
            );
        } else {
            self.pending_resume_seek = Some(latest_time);
            crate::vprintln!("[SEEK]   queued until player ready");
        }
    }

    fn handle_set_volume(&mut self, vol: f64) {
        let vol_f32 = (vol / 100.0) as f32;
        self.volume.store(f32::to_bits(vol_f32), Relaxed);

        #[cfg(target_os = "windows")]
        {
            if self.is_exclusive_mode {
                return;
            }
        }
    }

    fn handle_get_audio_devices(&self, req_id: Option<String>) {
        let devices = enumerate_audio_devices();
        (self.callback)(PlayerEvent::AudioDevices(devices, req_id));
    }

    fn handle_set_audio_device(&mut self, id: String, exclusive: bool) {
        let _ = &exclusive;
        #[cfg(target_os = "windows")]
        {
            if let Some(cancel) = self.exclusive_stream_cancel.take() {
                cancel.store(true, Relaxed);
            }
            if exclusive {
                // Switch to exclusive WASAPI
                self.stop_decode();
                self.cpal_stream = None;

                if let Some(old) = self.exclusive_handle.take() {
                    old.shutdown();
                }

                let handle = ExclusiveHandle::spawn(id.clone());
                self.is_exclusive_mode = true;
                self.exclusive_handle = Some(handle);

                // Reload current track in exclusive mode
                if let Some(ref handle) = self.exclusive_handle {
                    if let Some(ref buf) = self.current_buffer {
                        handle.send(ExclusiveCommand::Stop);
                        let cancel = Arc::new(AtomicBool::new(false));
                        self.exclusive_stream_cancel = Some(cancel.clone());

                        let cmd_tx = handle.command_sender();
                        let reader = buf.clone();
                        let total_len = buf.total_len();
                        let stream_id = EXCLUSIVE_STREAM_SEQ.fetch_add(1, Relaxed) + 1;
                        thread::spawn(move || {
                            if let Err(e) = wasapi::stream_flac_reader_to_wasapi(
                                reader,
                                total_len,
                                stream_id,
                                cmd_tx,
                                cancel.clone(),
                            ) && !cancel.load(Relaxed)
                            {
                                eprintln!("[WASAPI] Device switch stream decode failed: {e}");
                            }
                        });
                        self.has_track = true;
                        self.is_playing = true;
                    }
                }

                self.current_device_id = Some(id.clone());
                crate::vprintln!("[AUDIO] Switched to exclusive WASAPI: {}", id);
                return;
            } else if self.is_exclusive_mode {
                if let Some(old) = self.exclusive_handle.take() {
                    old.shutdown();
                }
                self.is_exclusive_mode = false;
                crate::vprintln!("[AUDIO] Switched back to shared mode");
            }
        }

        self.current_device_id = Some(id.clone());

        // Rebuild cpal stream on new device
        if self.has_track {
            let was_playing = self.is_playing;
            let position = self.current_position_secs();

            // Stop current decode + stream
            self.stop_decode();
            self.cpal_stream = None;

            // Re-load on new device by re-doing handle_load logic
            if let Some(ref buffer) = self.current_buffer {
                let buffer_clone = buffer.clone();
                // Reuse existing handle_load logic by sending a synthetic load
                // Actually, let's just rebuild the pipeline directly
                self.rebuild_pipeline_on_device(buffer_clone, was_playing, position);
            }
        }

        crate::vprintln!("[AUDIO] Switched to device: {}", id);
    }

    /// Rebuild the decode+cpal pipeline on the current device.
    fn rebuild_pipeline_on_device(&mut self, buffer: RamBuffer, was_playing: bool, seek_to: f64) {
        let _total_len = buffer.total_len();

        // Probe
        let probe_reader = buffer.clone();
        let mss = MediaSourceStream::new(Box::new(probe_reader), Default::default());
        let hint = Hint::new();

        let probed = match symphonia::default::get_probe().format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        ) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("[ERROR]  Probe failed on device switch: {e}");
                return;
            }
        };

        let track = match probed
            .format
            .tracks()
            .iter()
            .find(|t| t.codec_params.codec != symphonia::core::codecs::CODEC_TYPE_NULL)
        {
            Some(t) => t,
            None => {
                eprintln!("[ERROR]  No audio track on device switch");
                return;
            }
        };

        let sr = track.codec_params.sample_rate.unwrap_or(44100);
        let ch = track
            .codec_params
            .channels
            .map(|c| c.count() as u16)
            .unwrap_or(2);

        drop(probed);

        let device = if let Some(ref id) = self.current_device_id {
            match find_output_device(id) {
                Some(d) => d,
                None => {
                    crate::vprintln!("[AUDIO] Device '{}' not found, falling back to default", id);
                    (self.callback)(PlayerEvent::DeviceError("devicenotfound"));
                    match cpal::default_host().default_output_device() {
                        Some(d) => d,
                        None => {
                            (self.callback)(PlayerEvent::DeviceError("devicenotfound"));
                            return;
                        }
                    }
                }
            }
        } else {
            match cpal::default_host().default_output_device() {
                Some(d) => d,
                None => {
                    (self.callback)(PlayerEvent::DeviceError("devicenotfound"));
                    return;
                }
            }
        };

        let opened = match open_output_stream(&device, sr, ch, &self.volume) {
            Some(o) => o,
            None => {
                (self.callback)(PlayerEvent::DeviceError("deviceformatnotsupported"));
                return;
            }
        };

        let actual_rate = opened.rate;
        let actual_channels = opened.channels;
        self.cpal_muted = Some(opened.muted);
        self.cpal_stream_error = Some(opened.stream_error);

        self.sample_rate = actual_rate;
        self.channels = actual_channels;
        self.decoded_samples.store(0, Relaxed);

        let (decode_cmd_tx, decode_cmd_rx) = mpsc::channel();
        let (decode_event_tx, decode_event_rx) = mpsc::channel();
        let decoded_samples = self.decoded_samples.clone();

        let decode_buffer = buffer.clone();
        let decode_handle = spawn_decode_thread(
            decode_buffer,
            opened.producer,
            decoded_samples,
            decode_cmd_rx,
            decode_event_tx,
            actual_rate,
            actual_channels,
            opened.seek_gen,
        );

        let stream = opened.stream;

        self.cpal_stream = Some(stream);
        self.decode_cmd_tx = Some(decode_cmd_tx);
        self.decode_event_rx = Some(decode_event_rx);
        self.decode_handle = Some(decode_handle);
        self.current_buffer = Some(buffer);

        // Seek to previous position
        if seek_to > 0.0
            && let Some(ref tx) = self.decode_cmd_tx
        {
            let _ = tx.send(DecodeCommand::Seek(seek_to));
        }

        if was_playing {
            self.start_playback();
        }
    }

    /// Compute current playback position in seconds from decoded sample count.
    fn current_position_secs(&self) -> f64 {
        let samples = self.decoded_samples.load(Relaxed);
        let channels = self.channels.max(1) as u64;
        let frames = samples / channels;
        frames as f64 / self.sample_rate.max(1) as f64
    }

    #[cfg(target_os = "windows")]
    fn poll_exclusive_events(&mut self) {
        if self.is_exclusive_mode {
            if let Some(ref handle) = self.exclusive_handle {
                for ev in handle.poll_events() {
                    match ev {
                        ExclusiveEvent::TimeUpdate(t) => {
                            if let Some(track_id) = self.current_track_id.as_ref() {
                                self.resume_store.set(track_id, t);
                                self.resume_store.flush_if_due(false);
                            }
                            (self.callback)(PlayerEvent::TimeUpdate(t, self.current_seq));
                        }
                        ExclusiveEvent::StateChange(s) => {
                            if s == "completed" {
                                if let Some(track_id) = self.current_track_id.as_ref() {
                                    self.resume_store.clear(track_id);
                                    self.resume_store.flush_if_due(true);
                                }
                                self.has_track = false;
                                self.is_playing = false;
                                self.current_track_id = None;
                                crate::state::GOVERNOR
                                    .buffer_progress()
                                    .set_playback_active(false);
                            }
                            (self.callback)(PlayerEvent::StateChange(s, self.current_seq));
                        }
                        ExclusiveEvent::Duration(d) => {
                            self.current_duration = d;
                            (self.callback)(PlayerEvent::Duration(d, self.current_seq));
                            if let Some(seek_time) = self.pending_resume_seek.take() {
                                handle.send(ExclusiveCommand::Seek(seek_time));
                                (self.callback)(PlayerEvent::TimeUpdate(
                                    seek_time,
                                    self.current_seq,
                                ));
                            }
                        }
                        ExclusiveEvent::InitFailed(e) => {
                            eprintln!("[WASAPI] Init failed, falling back to shared mode: {e}");
                            if let Some(cancel) = self.exclusive_stream_cancel.take() {
                                cancel.store(true, Relaxed);
                            }
                            self.is_exclusive_mode = false;
                            (self.callback)(PlayerEvent::DeviceError(
                                "deviceexclusivemodenotallowed",
                            ));
                        }
                        ExclusiveEvent::DeviceLocked(e) => {
                            eprintln!("[WASAPI] Device locked by another process: {e}");
                            if let Some(cancel) = self.exclusive_stream_cancel.take() {
                                cancel.store(true, Relaxed);
                            }
                            self.is_exclusive_mode = false;
                            (self.callback)(PlayerEvent::DeviceError("devicelocked"));
                        }
                        ExclusiveEvent::Stopped => {
                            self.has_track = false;
                            self.is_playing = false;
                            self.current_track_id = None;
                            crate::state::GOVERNOR
                                .buffer_progress()
                                .set_playback_active(false);
                            self.resume_store.flush_if_due(true);
                        }
                    }
                }
            }

            if !self.is_exclusive_mode {
                self.exclusive_handle = None;
            }
        }
    }

    fn poll_playback(&mut self) {
        #[cfg(target_os = "windows")]
        let should_poll = !self.is_exclusive_mode;
        #[cfg(not(target_os = "windows"))]
        let should_poll = true;

        // Always update governor buffer progress when a track is loaded
        // (even when paused), so the governor can throttle downloads correctly.
        if should_poll
            && self.has_track
            && let Some(ref buf) = self.current_buffer
        {
            let bp = crate::state::GOVERNOR.buffer_progress();
            bp.written.store(buf.written(), Relaxed);
            bp.read_pos.store(buf.read_cursor(), Relaxed);
        }

        // Detect cpal stream errors (device disconnected, unknown, etc.)
        if let Some(ref flag) = self.cpal_stream_error {
            let code = flag.swap(0, Relaxed);
            if code == 1 {
                (self.callback)(PlayerEvent::DeviceError("devicedisconnected"));
            } else if code == 2 {
                (self.callback)(PlayerEvent::DeviceError("deviceunknownerror"));
            }
        }

        if !should_poll || !self.has_track || !self.is_playing {
            return;
        }

        // Detect decode thread stalling on RamBuffer (buffering).
        // Only relevant for streaming tracks (not cached), and not during seeks
        // (which already emit "seeking" state).
        if !self.is_cached
            && !self.seeking
            && let Some(ref buf) = self.current_buffer
        {
            let stalled = buf.is_stalled();
            if stalled && !self.buffer_stalled {
                self.buffer_stalled = true;
                (self.callback)(PlayerEvent::StateChange("idle", self.current_seq));
            } else if !stalled && self.buffer_stalled {
                self.buffer_stalled = false;
                (self.callback)(PlayerEvent::StateChange("active", self.current_seq));
            }
        }

        // Check decode events
        if let Some(ref rx) = self.decode_event_rx {
            while let Ok(event) = rx.try_recv() {
                match event {
                    DecodeEvent::Finished => {
                        crate::vprintln!(
                            "[TRACK]  Finished ({})",
                            if self.is_cached {
                                "from cache"
                            } else {
                                "from stream"
                            }
                        );
                        // Track completed
                        if let Some(track_id) = self.current_track_id.as_ref() {
                            self.resume_store.clear(track_id);
                            self.resume_store.flush_if_due(true);
                        }

                        // Write to disk cache if complete and not already cached
                        if !self.is_cached
                            && let Some(ref buf) = self.current_buffer
                            && buf.is_complete()
                            && let Some(data) = buf.take_data()
                        {
                            let track_id = self.current_track_id.clone();
                            std::thread::spawn(move || {
                                if let Some(tid) = track_id {
                                    let mut cache = crate::state::AUDIO_CACHE.lock().unwrap();
                                    match cache.store(&tid, "flac", &data) {
                                        Ok(()) => {
                                            crate::vprintln!(
                                                "[CACHE]  Stored: {} ({})",
                                                tid,
                                                super::format_bytes(data.len() as u64)
                                            );
                                        }
                                        Err(e) => {
                                            eprintln!("[CACHE]  Store failed: {e}");
                                        }
                                    }
                                }
                            });
                        }

                        (self.callback)(PlayerEvent::TimeUpdate(
                            self.current_duration,
                            self.current_seq,
                        ));
                        (self.callback)(PlayerEvent::StateChange("completed", self.current_seq));
                        self.has_track = false;
                        self.is_playing = false;
                        self.current_track_id = None;
                        crate::state::GOVERNOR
                            .buffer_progress()
                            .set_playback_active(false);
                        self.current_duration = 0.0;
                        return;
                    }
                    DecodeEvent::SeekComplete => {
                        // Always unmute (safe even if not muted)
                        if let Some(ref m) = self.cpal_muted {
                            m.store(false, Relaxed);
                        }
                        if self.seeking {
                            // User seek completed — log wall time
                            self.seeking = false;
                            self.seek_target = None;
                            let wall_ms = self
                                .seek_wall_start
                                .take()
                                .map(|s| s.elapsed().as_secs_f64() * 1000.0)
                                .unwrap_or(0.0);
                            crate::vprintln!(
                                "[SEEK]   complete: wall={} ({})",
                                format_ms(wall_ms),
                                if self.is_cached {
                                    "cached"
                                } else {
                                    "streaming"
                                }
                            );
                            // Signal that playback has resumed after the seek.
                            (self.callback)(PlayerEvent::StateChange("active", self.current_seq));
                        }
                        // Resume seeks (from handle_play) also emit SeekComplete
                        // but don't set seeking=true — no wall timer logged.
                    }
                    DecodeEvent::Error(e) => {
                        crate::vprintln!("[DECODE] Error: {e}");
                        (self.callback)(PlayerEvent::MediaError {
                            error: e,
                            code: "unreadable_file",
                        });
                    }
                }
            }
        }

        // Emit time update — during seeking, pin the display at the target position
        // so the frontend's seek bar doesn't freeze or revert.
        if let Some(target) = self.seek_target {
            (self.callback)(PlayerEvent::TimeUpdate(target, self.current_seq));
        } else {
            let pos_secs = self.current_position_secs();
            if pos_secs > 0.0 {
                if let Some(track_id) = self.current_track_id.as_ref() {
                    self.resume_store.set(track_id, pos_secs);
                    self.resume_store.flush_if_due(false);
                }
                (self.callback)(PlayerEvent::TimeUpdate(pos_secs, self.current_seq));
            }
        }
    }
}
