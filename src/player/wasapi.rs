use std::io::{Read, Seek};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use symphonia::core::audio::RawSampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use wasapi::{
    AudioClient, AudioRenderClient, DeviceEnumerator, Direction, Handle, SampleType, StreamMode,
    WaveFormat, calculate_period_100ns,
};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

pub(super) enum ExclusiveCommand {
    StartStream {
        stream_id: u32,
        sample_rate: u32,
        channels: u32,
        bits_per_sample: u32,
        duration_secs: f64,
    },
    PushPcm {
        stream_id: u32,
        pcm_data: Vec<u8>,
    },
    EndStream {
        stream_id: u32,
    },
    Play,
    Pause,
    Stop,
    Seek(f64),
    Shutdown,
}

pub(super) enum ExclusiveEvent {
    TimeUpdate(f64),
    StateChange(super::PlaybackState),
    Duration(f64),
    InitFailed(String),
    DeviceLocked(String),
    Stopped,
}

struct SizedMediaSource<R> {
    inner: R,
    byte_len: u64,
}

impl<R: Read> Read for SizedMediaSource<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.inner.read(buf)
    }
}

impl<R: Seek> Seek for SizedMediaSource<R> {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        self.inner.seek(pos)
    }
}

impl<R: Read + Seek + Send + Sync> MediaSource for SizedMediaSource<R> {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        Some(self.byte_len)
    }
}

pub(super) struct ExclusiveHandle {
    cmd_tx: mpsc::Sender<ExclusiveCommand>,
    event_rx: mpsc::Receiver<ExclusiveEvent>,
    thread: Option<JoinHandle<()>>,
}

// ---------------------------------------------------------------------------
// FLAC -> PCM decoding (symphonia)
// ---------------------------------------------------------------------------

fn append_interleaved_i32_as_pcm(raw_bytes: &[u8], bits_per_sample: u32, out: &mut Vec<u8>) {
    if bits_per_sample <= 16 {
        // i32 samples -> take upper 16 bits (little-endian: bytes [2..4]).
        let frame_count = raw_bytes.len() / 4;
        out.reserve(frame_count * 2);
        for i in 0..frame_count {
            let offset = i * 4;
            out.push(raw_bytes[offset + 2]);
            out.push(raw_bytes[offset + 3]);
        }
    } else if bits_per_sample <= 24 {
        // i32 samples -> take upper 24 bits (bytes [1..4]).
        let frame_count = raw_bytes.len() / 4;
        out.reserve(frame_count * 3);
        for i in 0..frame_count {
            let offset = i * 4;
            out.push(raw_bytes[offset + 1]);
            out.push(raw_bytes[offset + 2]);
            out.push(raw_bytes[offset + 3]);
        }
    } else {
        // 32-bit: pass through all 4 bytes.
        out.extend_from_slice(raw_bytes);
    }
}

pub(super) fn stream_flac_reader_to_wasapi<R>(
    reader: R,
    byte_len: u64,
    stream_id: u32,
    cmd_tx: mpsc::Sender<ExclusiveCommand>,
    cancel: Arc<AtomicBool>,
) -> Result<(), String>
where
    R: Read + Seek + Send + Sync + 'static,
{
    let source = Box::new(SizedMediaSource {
        inner: reader,
        byte_len,
    });
    let mss = MediaSourceStream::new(source, Default::default());

    let mut hint = Hint::new();
    hint.with_extension("flac");

    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|e| format!("probe failed: {e}"))?;

    let mut format_reader = probed.format;
    let track = format_reader
        .default_track()
        .ok_or("no default track")?
        .clone();

    let codec_params = &track.codec_params;
    let sample_rate = codec_params.sample_rate.ok_or("no sample rate")?;
    let channels = codec_params.channels.ok_or("no channel info")?.count() as u32;
    let bits_per_sample = codec_params.bits_per_sample.ok_or("no bits_per_sample")?;
    let n_frames = codec_params.n_frames.unwrap_or(0);
    let duration_secs = if sample_rate > 0 && n_frames > 0 {
        n_frames as f64 / sample_rate as f64
    } else {
        0.0
    };

    let stored_bps = if bits_per_sample <= 16 {
        16
    } else if bits_per_sample <= 24 {
        24
    } else {
        32
    };

    cmd_tx
        .send(ExclusiveCommand::StartStream {
            stream_id,
            sample_rate,
            channels,
            bits_per_sample: stored_bps,
            duration_secs,
        })
        .map_err(|_| "failed to send StartStream".to_string())?;

    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| format!("decoder creation failed: {e}"))?;

    loop {
        if cancel.load(Relaxed) {
            return Ok(());
        }

        let packet = match format_reader.next_packet() {
            Ok(p) => p,
            Err(symphonia::core::errors::Error::IoError(ref e))
                if e.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break;
            }
            Err(symphonia::core::errors::Error::IoError(e)) => {
                if cancel.load(Relaxed) {
                    return Ok(());
                }
                return Err(format!("decode io error: {e}"));
            }
            Err(e) => {
                if cancel.load(Relaxed) {
                    return Ok(());
                }
                return Err(format!("decode packet error: {e}"));
            }
        };

        if packet.track_id() != track.id {
            continue;
        }

        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(_) => continue,
        };

        let spec = *decoded.spec();
        let num_frames = decoded.capacity();
        let mut raw_buf = RawSampleBuffer::<i32>::new(num_frames as u64, spec);
        raw_buf.copy_interleaved_ref(decoded);

        let mut chunk = Vec::new();
        append_interleaved_i32_as_pcm(raw_buf.as_bytes(), bits_per_sample, &mut chunk);

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
    }

    if !cancel.load(Relaxed) {
        let _ = cmd_tx.send(ExclusiveCommand::EndStream { stream_id });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// ExclusiveHandle – public API
// ---------------------------------------------------------------------------

impl ExclusiveHandle {
    /// Spawn the WASAPI render thread for the given device.
    /// `device_id` – wasapi device id string, or "default".
    pub fn spawn(device_id: String) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel::<ExclusiveCommand>();
        let (event_tx, event_rx) = mpsc::channel::<ExclusiveEvent>();

        let handle = thread::spawn(move || {
            render_thread(device_id, cmd_rx, event_tx);
        });

        Self {
            cmd_tx,
            event_rx,
            thread: Some(handle),
        }
    }

    pub fn send(&self, cmd: ExclusiveCommand) {
        let _ = self.cmd_tx.send(cmd);
    }

    pub fn command_sender(&self) -> mpsc::Sender<ExclusiveCommand> {
        self.cmd_tx.clone()
    }

    /// Drain all pending events (non-blocking).
    pub fn poll_events(&self) -> Vec<ExclusiveEvent> {
        let mut events = Vec::new();
        while let Ok(ev) = self.event_rx.try_recv() {
            events.push(ev);
        }
        events
    }

    pub fn shutdown(mut self) {
        let _ = self.cmd_tx.send(ExclusiveCommand::Shutdown);
        if let Some(h) = self.thread.take() {
            let _ = h.join();
        }
    }
}

impl Drop for ExclusiveHandle {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(ExclusiveCommand::Shutdown);
        if let Some(h) = self.thread.take() {
            let _ = h.join();
        }
    }
}

// ---------------------------------------------------------------------------
// Format negotiation helpers
// ---------------------------------------------------------------------------

fn negotiate_format(sample_rate: u32, channels: u32, source_bps: u32) -> Vec<(WaveFormat, i64)> {
    let sr = sample_rate as usize;
    let ch = channels as usize;
    let bps = source_bps as usize;

    let channel_mask = wasapi::make_channelmasks(ch).first().copied();

    let mut candidates = Vec::new();

    // Priority 1: 32-bit container with source valid bits (integer)
    candidates.push(WaveFormat::new(
        32,
        bps.min(32),
        &SampleType::Int,
        sr,
        ch,
        channel_mask,
    ));
    // Priority 2: 24-bit container with 24 valid bits
    if bps != 24 {
        candidates.push(WaveFormat::new(
            24,
            24,
            &SampleType::Int,
            sr,
            ch,
            channel_mask,
        ));
    }
    // Priority 3: 16-bit container with 16 valid bits
    if bps != 16 {
        candidates.push(WaveFormat::new(
            16,
            16,
            &SampleType::Int,
            sr,
            ch,
            channel_mask,
        ));
    }
    // Priority 4: 32-bit float
    candidates.push(WaveFormat::new(
        32,
        32,
        &SampleType::Float,
        sr,
        ch,
        channel_mask,
    ));

    let period = calculate_period_100ns(
        (sr as i64) / 100, // ~10ms buffer
        sr as i64,
    );

    candidates.into_iter().map(|fmt| (fmt, period)).collect()
}

fn init_exclusive_client(device_id: &str) -> Result<(DeviceEnumerator, wasapi::Device), String> {
    let enumerator = DeviceEnumerator::new().map_err(|e| format!("DeviceEnumerator: {e}"))?;

    let device = if device_id == "default" {
        enumerator
            .get_default_device(&Direction::Render)
            .map_err(|e| format!("default device: {e}"))?
    } else {
        enumerator
            .get_device(device_id)
            .map_err(|e| format!("device '{device_id}': {e}"))?
    };

    Ok((enumerator, device))
}

fn open_exclusive_stream(
    device: &wasapi::Device,
    sample_rate: u32,
    channels: u32,
    source_bps: u32,
) -> Result<(AudioClient, AudioRenderClient, Handle, WaveFormat, u32), String> {
    let candidates = negotiate_format(sample_rate, channels, source_bps);

    for (wave_fmt, period) in &candidates {
        let mut audio_client = device
            .get_iaudioclient()
            .map_err(|e| format!("get_iaudioclient: {e}"))?;

        let stream_mode = StreamMode::EventsExclusive {
            period_hns: *period,
        };

        match audio_client.initialize_client(wave_fmt, &Direction::Render, &stream_mode) {
            Ok(()) => {
                let h_event = audio_client
                    .set_get_eventhandle()
                    .map_err(|e| format!("eventhandle: {e}"))?;
                let buffer_size = audio_client
                    .get_buffer_size()
                    .map_err(|e| format!("buffer_size: {e}"))?;
                let render_client = audio_client
                    .get_audiorenderclient()
                    .map_err(|e| format!("render_client: {e}"))?;

                crate::vprintln!(
                    "[WASAPI] Exclusive stream opened: {}Hz {}ch {}bit, buffer={}frames",
                    sample_rate,
                    channels,
                    wave_fmt.get_validbitspersample(),
                    buffer_size
                );

                return Ok((
                    audio_client,
                    render_client,
                    h_event,
                    wave_fmt.clone(),
                    buffer_size,
                ));
            }
            Err(e) => {
                let err_str = format!("{e}");
                // Handle AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED
                if err_str.contains("BUFFER_SIZE_NOT_ALIGNED") || err_str.contains("88890019") {
                    // Get aligned size and retry
                    if let Ok(aligned_size) = audio_client.get_buffer_size() {
                        let aligned_period =
                            calculate_period_100ns(aligned_size as i64, sample_rate as i64);

                        drop(audio_client);
                        let mut audio_client2 = device
                            .get_iaudioclient()
                            .map_err(|e| format!("get_iaudioclient retry: {e}"))?;

                        let stream_mode2 = StreamMode::EventsExclusive {
                            period_hns: aligned_period,
                        };

                        if audio_client2
                            .initialize_client(wave_fmt, &Direction::Render, &stream_mode2)
                            .is_ok()
                        {
                            let h_event = audio_client2
                                .set_get_eventhandle()
                                .map_err(|e| format!("eventhandle: {e}"))?;
                            let buffer_size = audio_client2
                                .get_buffer_size()
                                .map_err(|e| format!("buffer_size: {e}"))?;
                            let render_client = audio_client2
                                .get_audiorenderclient()
                                .map_err(|e| format!("render_client: {e}"))?;

                            crate::vprintln!(
                                "[WASAPI] Exclusive stream opened (aligned): {}Hz {}ch {}bit, buffer={}frames",
                                sample_rate,
                                channels,
                                wave_fmt.get_validbitspersample(),
                                buffer_size
                            );

                            return Ok((
                                audio_client2,
                                render_client,
                                h_event,
                                wave_fmt.clone(),
                                buffer_size,
                            ));
                        }
                    }
                }
                crate::vprintln!("[WASAPI] Format rejected: {e}");
                continue;
            }
        }
    }

    Err("no compatible exclusive format found".to_string())
}

/// Check if a WASAPI error indicates the device is locked by another process.
fn is_device_in_use_error(err_str: &str) -> bool {
    err_str.contains("DEVICE_IN_USE")
        || err_str.contains("8889000a")
        || err_str.contains("8889000A")
}

// ---------------------------------------------------------------------------
// PCM conversion helpers
// ---------------------------------------------------------------------------

/// Convert source PCM bytes to the format expected by the WASAPI device.
/// Source is always integer PCM at `src_bps` bits, output is at `dst_store_bits`/`dst_valid_bits`.
fn convert_pcm_frame(
    src: &[u8],
    src_bps: u32,
    dst_store_bits: u32,
    _dst_valid_bits: u32,
    dst_sample_type: &SampleType,
    _channels: u32,
) -> Vec<u8> {
    let src_bytes_per_sample = (src_bps / 8) as usize;
    let dst_bytes_per_sample = (dst_store_bits / 8) as usize;
    let num_samples = src.len() / src_bytes_per_sample;
    let mut out = Vec::with_capacity(num_samples * dst_bytes_per_sample);

    for i in 0..num_samples {
        let offset = i * src_bytes_per_sample;
        let sample_bytes = &src[offset..offset + src_bytes_per_sample];

        // Read source sample as i32 (sign-extended)
        let sample_i32: i32 = match src_bps {
            16 => {
                let val = i16::from_le_bytes([sample_bytes[0], sample_bytes[1]]);
                val as i32
            }
            24 => {
                let val = (sample_bytes[0] as i32)
                    | ((sample_bytes[1] as i32) << 8)
                    | ((sample_bytes[2] as i32) << 16);
                // Sign extend from 24 bits
                if val & 0x800000 != 0 {
                    val | !0xFFFFFF
                } else {
                    val
                }
            }
            32 => i32::from_le_bytes([
                sample_bytes[0],
                sample_bytes[1],
                sample_bytes[2],
                sample_bytes[3],
            ]),
            _ => 0,
        };

        match dst_sample_type {
            SampleType::Float => {
                // Convert to f32 normalized [-1.0, 1.0]
                let max_val = (1i64 << (src_bps - 1)) as f32;
                let f = (sample_i32 as f32) / max_val;
                out.extend_from_slice(&f.to_le_bytes());
            }
            SampleType::Int => match dst_store_bits {
                16 => {
                    let val = match src_bps {
                        16 => sample_i32 as i16,
                        24 => (sample_i32 >> 8) as i16,
                        32 => (sample_i32 >> 16) as i16,
                        _ => 0,
                    };
                    out.extend_from_slice(&val.to_le_bytes());
                }
                24 => {
                    let val = match src_bps {
                        16 => sample_i32 << 8,
                        24 => sample_i32,
                        32 => sample_i32 >> 8,
                        _ => 0,
                    };
                    out.push((val & 0xFF) as u8);
                    out.push(((val >> 8) & 0xFF) as u8);
                    out.push(((val >> 16) & 0xFF) as u8);
                }
                32 => {
                    let val = match src_bps {
                        16 => sample_i32 << 16,
                        24 => sample_i32 << 8,
                        32 => sample_i32,
                        _ => 0,
                    };
                    out.extend_from_slice(&val.to_le_bytes());
                }
                _ => {}
            },
        }
    }

    out
}

// ---------------------------------------------------------------------------
// Render thread
// ---------------------------------------------------------------------------

enum RenderState {
    Idle,
    Playing,
    Paused,
}

/// Compute the write cursor and frames_played for a seek to `time` seconds.
fn compute_seek_position(
    time: f64,
    sample_rate: u32,
    channels: u32,
    src_bps: u32,
    pcm_len: usize,
) -> (usize, u64) {
    let src_bytes_per_sample = (src_bps / 8) as usize;
    let src_bytes_per_frame = src_bytes_per_sample * channels as usize;
    let target_frame = (time * sample_rate as f64) as u64;
    let mut cursor = (target_frame as usize) * src_bytes_per_frame;
    if cursor > pcm_len {
        cursor = pcm_len;
    }
    let frames = if src_bytes_per_frame > 0 {
        (cursor / src_bytes_per_frame) as u64
    } else {
        0
    };
    (cursor, frames)
}

/// Stop the current stream (if any), open a new exclusive stream, and dispatch
/// error events on failure. Returns Ok with the new resources, or Err(()) when
/// an error event has been sent and the render thread should exit.
fn try_open_stream(
    device: &wasapi::Device,
    sample_rate: u32,
    channels: u32,
    bits_per_sample: u32,
    audio_client: &Option<AudioClient>,
    event_tx: &mpsc::Sender<ExclusiveEvent>,
) -> Result<(AudioClient, AudioRenderClient, Handle, WaveFormat, u32), ()> {
    if let Some(ac) = audio_client {
        let _ = ac.stop_stream();
    }

    match open_exclusive_stream(device, sample_rate, channels, bits_per_sample) {
        Ok(resources) => Ok(resources),
        Err(e) => {
            eprintln!("[WASAPI] Failed to open stream: {e}");
            if is_device_in_use_error(&e) {
                let _ = event_tx.send(ExclusiveEvent::DeviceLocked(e));
            } else {
                let _ = event_tx.send(ExclusiveEvent::InitFailed(e));
            }
            Err(())
        }
    }
}

struct RenderContext {
    audio_client: Option<AudioClient>,
    render_client: Option<AudioRenderClient>,
    h_event: Option<Handle>,
    wave_fmt: Option<WaveFormat>,
    buffer_size: u32,
    pcm_data: Vec<u8>,
    pcm_sample_rate: u32,
    pcm_channels: u32,
    pcm_src_bps: u32,
    pcm_duration: f64,
    write_cursor: usize,
    frames_played: u64,
    current_stream_id: Option<u32>,
    stream_ended: bool,
    last_time_report: Instant,
    state: RenderState,
}

impl RenderContext {
    fn new() -> Self {
        Self {
            audio_client: None,
            render_client: None,
            h_event: None,
            wave_fmt: None,
            buffer_size: 0,
            pcm_data: Vec::new(),
            pcm_sample_rate: 0,
            pcm_channels: 0,
            pcm_src_bps: 0,
            pcm_duration: 0.0,
            write_cursor: 0,
            frames_played: 0,
            current_stream_id: None,
            stream_ended: true,
            last_time_report: Instant::now(),
            state: RenderState::Idle,
        }
    }

    fn stop_audio_client(&self) {
        if let Some(ref ac) = self.audio_client {
            let _ = ac.stop_stream();
        }
    }

    /// Open a new stream, reset all PCM/playback state, start playback.
    /// Postconditions on Ok: pcm_data cleared, cursors zeroed, stream_id set,
    /// audio resources replaced, state = Playing, Duration + Active events sent.
    /// On Err: an error event was sent and render_thread must exit.
    fn handle_start_stream(
        &mut self,
        device: &wasapi::Device,
        event_tx: &mpsc::Sender<ExclusiveEvent>,
        stream_id: u32,
        sample_rate: u32,
        channels: u32,
        bits_per_sample: u32,
        duration_secs: f64,
    ) -> Result<(), ()> {
        let (ac, rc, ev, wf, bs) = try_open_stream(
            device,
            sample_rate,
            channels,
            bits_per_sample,
            &self.audio_client,
            event_tx,
        )?;

        self.pcm_data.clear();
        self.pcm_sample_rate = sample_rate;
        self.pcm_channels = channels;
        self.pcm_src_bps = bits_per_sample;
        self.pcm_duration = duration_secs;
        self.write_cursor = 0;
        self.frames_played = 0;
        self.current_stream_id = Some(stream_id);
        self.stream_ended = false;
        self.buffer_size = bs;
        self.last_time_report = Instant::now();

        self.audio_client = Some(ac);
        self.render_client = Some(rc);
        self.h_event = Some(ev);
        self.wave_fmt = Some(wf);

        let _ = event_tx.send(ExclusiveEvent::Duration(duration_secs));
        if let Some(ref ac) = self.audio_client {
            let _ = ac.start_stream();
        }
        self.state = RenderState::Playing;
        let _ = event_tx.send(ExclusiveEvent::StateChange(super::PlaybackState::Active));
        Ok(())
    }

    /// Stop playback and reset all track state.
    /// Postconditions: audio stopped, pcm_data cleared, cursors zeroed,
    /// stream_id = None, stream_ended = true, state = Idle, Stopped event sent.
    fn handle_stop(&mut self, event_tx: &mpsc::Sender<ExclusiveEvent>) {
        self.stop_audio_client();
        self.pcm_data.clear();
        self.write_cursor = 0;
        self.frames_played = 0;
        self.current_stream_id = None;
        self.stream_ended = true;
        self.state = RenderState::Idle;
        let _ = event_tx.send(ExclusiveEvent::Stopped);
    }

    fn handle_push_pcm(&mut self, stream_id: u32, data: Vec<u8>) {
        if self.current_stream_id == Some(stream_id) {
            self.pcm_data.extend_from_slice(&data);
        }
    }

    fn handle_end_stream(&mut self, stream_id: u32) {
        if self.current_stream_id == Some(stream_id) {
            self.stream_ended = true;
        }
    }
}

fn render_thread(
    device_id: String,
    cmd_rx: mpsc::Receiver<ExclusiveCommand>,
    event_tx: mpsc::Sender<ExclusiveEvent>,
) {
    let _ = render_thread_inner(device_id, cmd_rx, event_tx);
}

fn render_thread_inner(
    device_id: String,
    cmd_rx: mpsc::Receiver<ExclusiveCommand>,
    event_tx: mpsc::Sender<ExclusiveEvent>,
) -> Result<(), ()> {
    let hr = wasapi::initialize_mta();
    if hr.is_err() {
        let _ = event_tx.send(ExclusiveEvent::InitFailed(format!("COM init: {hr}")));
        return Err(());
    }

    let (_enumerator, device) = match init_exclusive_client(&device_id) {
        Ok(v) => v,
        Err(e) => {
            let _ = event_tx.send(ExclusiveEvent::InitFailed(e));
            return Err(());
        }
    };

    let mut ctx = RenderContext::new();

    loop {
        match ctx.state {
            RenderState::Idle => {
                match cmd_rx.recv() {
                    Ok(ExclusiveCommand::StartStream {
                        stream_id,
                        sample_rate,
                        channels,
                        bits_per_sample,
                        duration_secs,
                    }) => {
                        ctx.handle_start_stream(
                            &device,
                            &event_tx,
                            stream_id,
                            sample_rate,
                            channels,
                            bits_per_sample,
                            duration_secs,
                        )?;
                    }
                    Ok(ExclusiveCommand::Shutdown) | Err(_) => break,
                    _ => {} // Ignore other commands in idle
                }
            }

            RenderState::Playing => {
                while let Ok(cmd) = cmd_rx.try_recv() {
                    match cmd {
                        ExclusiveCommand::Pause => {
                            ctx.stop_audio_client();
                            ctx.state = RenderState::Paused;
                            let _ = event_tx
                                .send(ExclusiveEvent::StateChange(super::PlaybackState::Paused));
                        }
                        ExclusiveCommand::Stop => ctx.handle_stop(&event_tx),
                        ExclusiveCommand::Seek(time) => {
                            if ctx.wave_fmt.is_some() {
                                ctx.stop_audio_client();
                                (ctx.write_cursor, ctx.frames_played) = compute_seek_position(
                                    time,
                                    ctx.pcm_sample_rate,
                                    ctx.pcm_channels,
                                    ctx.pcm_src_bps,
                                    ctx.pcm_data.len(),
                                );
                                if let Some(ref ac) = ctx.audio_client {
                                    let _ = ac.start_stream();
                                }
                            }
                        }
                        ExclusiveCommand::StartStream {
                            stream_id,
                            sample_rate,
                            channels,
                            bits_per_sample,
                            duration_secs,
                        } => {
                            ctx.handle_start_stream(
                                &device,
                                &event_tx,
                                stream_id,
                                sample_rate,
                                channels,
                                bits_per_sample,
                                duration_secs,
                            )?;
                        }
                        ExclusiveCommand::PushPcm {
                            stream_id,
                            pcm_data: data,
                        } => ctx.handle_push_pcm(stream_id, data),
                        ExclusiveCommand::EndStream { stream_id } => {
                            ctx.handle_end_stream(stream_id)
                        }
                        ExclusiveCommand::Shutdown => {
                            ctx.stop_audio_client();
                            return Ok(());
                        }
                        _ => {}
                    }
                }

                if !matches!(ctx.state, RenderState::Playing) {
                    continue;
                }

                if let Some(ref ev) = ctx.h_event {
                    let _ = ev.wait_for_event(50);
                }

                if let (Some(ac), Some(rc), Some(wf)) =
                    (&ctx.audio_client, &ctx.render_client, &ctx.wave_fmt)
                {
                    let available = match ac.get_available_space_in_frames() {
                        Ok(n) => n as usize,
                        Err(_) => continue,
                    };

                    if available == 0 {
                        continue;
                    }

                    let src_bytes_per_sample = (ctx.pcm_src_bps / 8) as usize;
                    let src_bytes_per_frame = src_bytes_per_sample * ctx.pcm_channels as usize;
                    let dst_bytes_per_sample = wf.get_bitspersample() as usize / 8;
                    let dst_bytes_per_frame = dst_bytes_per_sample * ctx.pcm_channels as usize;

                    let remaining_src_bytes = ctx.pcm_data.len().saturating_sub(ctx.write_cursor);
                    let remaining_frames = if src_bytes_per_frame > 0 {
                        remaining_src_bytes / src_bytes_per_frame
                    } else {
                        0
                    };

                    if remaining_frames == 0 {
                        let silence = vec![0u8; available * dst_bytes_per_frame];
                        let _ = rc.write_to_device(available, &silence, None);

                        if !ctx.stream_ended {
                            continue;
                        }

                        let _ = event_tx.send(ExclusiveEvent::TimeUpdate(ctx.pcm_duration));
                        let _ = event_tx
                            .send(ExclusiveEvent::StateChange(super::PlaybackState::Completed));

                        ctx.stop_audio_client();
                        ctx.current_stream_id = None;
                        ctx.stream_ended = true;
                        ctx.state = RenderState::Idle;
                        continue;
                    }

                    let frames_to_write = available.min(remaining_frames);
                    let src_chunk_size = frames_to_write * src_bytes_per_frame;
                    let src_chunk =
                        &ctx.pcm_data[ctx.write_cursor..ctx.write_cursor + src_chunk_size];

                    let dst_store = wf.get_bitspersample() as u32;
                    let dst_valid = wf.get_validbitspersample() as u32;
                    let dst_type = wf.get_subformat().unwrap_or(SampleType::Int);

                    let write_data: std::borrow::Cow<'_, [u8]> =
                        if ctx.pcm_src_bps == dst_store && matches!(dst_type, SampleType::Int) {
                            std::borrow::Cow::Borrowed(src_chunk)
                        } else {
                            std::borrow::Cow::Owned(convert_pcm_frame(
                                src_chunk,
                                ctx.pcm_src_bps,
                                dst_store,
                                dst_valid,
                                &dst_type,
                                ctx.pcm_channels,
                            ))
                        };

                    if let Err(e) = rc.write_to_device(frames_to_write, &write_data, None) {
                        crate::vprintln!("[WASAPI] write error: {e}");
                    }

                    ctx.write_cursor += src_chunk_size;
                    ctx.frames_played += frames_to_write as u64;

                    if ctx.last_time_report.elapsed().as_millis() >= 200 {
                        let pos = ctx.frames_played as f64 / ctx.pcm_sample_rate as f64;
                        let _ = event_tx.send(ExclusiveEvent::TimeUpdate(pos));
                        ctx.last_time_report = Instant::now();
                    }
                }
            }

            RenderState::Paused => match cmd_rx.recv() {
                Ok(ExclusiveCommand::Play) => {
                    if let Some(ref ac) = ctx.audio_client {
                        let _ = ac.start_stream();
                    }
                    ctx.state = RenderState::Playing;
                    let _ =
                        event_tx.send(ExclusiveEvent::StateChange(super::PlaybackState::Active));
                }
                Ok(ExclusiveCommand::Stop) => ctx.handle_stop(&event_tx),
                Ok(ExclusiveCommand::Seek(time)) => {
                    if ctx.wave_fmt.is_some() {
                        (ctx.write_cursor, ctx.frames_played) = compute_seek_position(
                            time,
                            ctx.pcm_sample_rate,
                            ctx.pcm_channels,
                            ctx.pcm_src_bps,
                            ctx.pcm_data.len(),
                        );
                    }
                }
                Ok(ExclusiveCommand::StartStream {
                    stream_id,
                    sample_rate,
                    channels,
                    bits_per_sample,
                    duration_secs,
                }) => {
                    ctx.handle_start_stream(
                        &device,
                        &event_tx,
                        stream_id,
                        sample_rate,
                        channels,
                        bits_per_sample,
                        duration_secs,
                    )?;
                }
                Ok(ExclusiveCommand::PushPcm {
                    stream_id,
                    pcm_data: data,
                }) => ctx.handle_push_pcm(stream_id, data),
                Ok(ExclusiveCommand::EndStream { stream_id }) => ctx.handle_end_stream(stream_id),
                Ok(ExclusiveCommand::Shutdown) | Err(_) => {
                    ctx.stop_audio_client();
                    return Ok(());
                }
                _ => {}
            },
        }
    }

    Ok(())
}
