use std::io::Cursor;
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::Instant;

use symphonia::core::audio::RawSampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use wasapi::{
    AudioClient, AudioRenderClient, DeviceEnumerator, Direction, Handle, SampleType, StreamMode,
    WaveFormat, calculate_period_100ns,
};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

pub enum ExclusiveCommand {
    Load {
        pcm_data: Vec<u8>,
        sample_rate: u32,
        channels: u32,
        bits_per_sample: u32,
        total_frames: u64,
        duration_secs: f64,
    },
    Play,
    Pause,
    Stop,
    Seek(f64),
    Shutdown,
}

pub enum ExclusiveEvent {
    TimeUpdate(f64),
    StateChange(String),
    Duration(f64),
    InitFailed(String),
    Stopped,
}

pub struct DecodedPcm {
    pub data: Vec<u8>,
    pub sample_rate: u32,
    pub channels: u32,
    pub bits_per_sample: u32,
    pub total_frames: u64,
    pub duration_secs: f64,
}

pub struct ExclusiveHandle {
    cmd_tx: mpsc::Sender<ExclusiveCommand>,
    event_rx: mpsc::Receiver<ExclusiveEvent>,
    thread: Option<JoinHandle<()>>,
}

// ---------------------------------------------------------------------------
// FLAC -> PCM decoding (symphonia)
// ---------------------------------------------------------------------------

pub fn decode_flac_to_pcm(raw: &[u8]) -> Result<DecodedPcm, String> {
    let cursor = Cursor::new(raw.to_vec());
    let mss = MediaSourceStream::new(Box::new(cursor), Default::default());

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
    let channels = codec_params
        .channels
        .ok_or("no channel info")?
        .count() as u32;
    let bits_per_sample = codec_params.bits_per_sample.ok_or("no bits_per_sample")?;
    let n_frames = codec_params.n_frames.unwrap_or(0);

    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| format!("decoder creation failed: {e}"))?;

    let bytes_per_sample = ((bits_per_sample + 7) / 8).max(2) as usize;
    let estimated_size = n_frames as usize * channels as usize * bytes_per_sample;
    let mut pcm_bytes: Vec<u8> = Vec::with_capacity(estimated_size);
    let mut total_decoded_frames: u64 = 0;

    loop {
        let packet = match format_reader.next_packet() {
            Ok(p) => p,
            Err(symphonia::core::errors::Error::IoError(ref e))
                if e.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break;
            }
            Err(_) => break,
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

        // Use RawSampleBuffer to get interleaved bytes at the native bit depth.
        // We always decode as i32 and then truncate to the target container size.
        let mut raw_buf = RawSampleBuffer::<i32>::new(num_frames as u64, spec);
        raw_buf.copy_interleaved_ref(decoded);

        let raw_bytes = raw_buf.as_bytes();

        if bits_per_sample <= 16 {
            // i32 samples → take the upper 16 bits (little-endian: bytes [2..4])
            let frame_count = raw_bytes.len() / 4;
            for i in 0..frame_count {
                let offset = i * 4;
                pcm_bytes.push(raw_bytes[offset + 2]);
                pcm_bytes.push(raw_bytes[offset + 3]);
            }
        } else if bits_per_sample <= 24 {
            // i32 samples → take upper 24 bits (bytes [1..4])
            let frame_count = raw_bytes.len() / 4;
            for i in 0..frame_count {
                let offset = i * 4;
                pcm_bytes.push(raw_bytes[offset + 1]);
                pcm_bytes.push(raw_bytes[offset + 2]);
                pcm_bytes.push(raw_bytes[offset + 3]);
            }
        } else {
            // 32-bit: pass through all 4 bytes
            pcm_bytes.extend_from_slice(raw_bytes);
        }

        total_decoded_frames += num_frames as u64;
    }

    let duration_secs = if sample_rate > 0 {
        total_decoded_frames as f64 / sample_rate as f64
    } else {
        0.0
    };

    // The actual bits stored per sample in our output buffer
    let stored_bps = if bits_per_sample <= 16 {
        16
    } else if bits_per_sample <= 24 {
        24
    } else {
        32
    };

    Ok(DecodedPcm {
        data: pcm_bytes,
        sample_rate,
        channels,
        bits_per_sample: stored_bps,
        total_frames: total_decoded_frames,
        duration_secs,
    })
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

fn negotiate_format(
    sample_rate: u32,
    channels: u32,
    source_bps: u32,
) -> Vec<(WaveFormat, i64)> {
    let sr = sample_rate as usize;
    let ch = channels as usize;
    let bps = source_bps as usize;

    let channel_mask = wasapi::make_channelmasks(ch)
        .first().copied();

    let mut candidates = Vec::new();

    // Priority 1: 32-bit container with source valid bits (integer)
    candidates.push(WaveFormat::new(32, bps.min(32), &SampleType::Int, sr, ch, channel_mask));
    // Priority 2: 24-bit container with 24 valid bits
    if bps != 24 {
        candidates.push(WaveFormat::new(24, 24, &SampleType::Int, sr, ch, channel_mask));
    }
    // Priority 3: 16-bit container with 16 valid bits
    if bps != 16 {
        candidates.push(WaveFormat::new(16, 16, &SampleType::Int, sr, ch, channel_mask));
    }
    // Priority 4: 32-bit float
    candidates.push(WaveFormat::new(32, 32, &SampleType::Float, sr, ch, channel_mask));

    let period = calculate_period_100ns(
        (sr as i64) / 100, // ~10ms buffer
        sr as i64,
    );

    candidates.into_iter().map(|fmt| (fmt, period)).collect()
}

fn init_exclusive_client(
    device_id: &str,
) -> Result<(DeviceEnumerator, wasapi::Device), String> {
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

        match audio_client.initialize_client(&wave_fmt, &Direction::Render, &stream_mode) {
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

                eprintln!(
                    "[WASAPI] Exclusive stream opened: {}Hz {}ch {}bit, buffer={}frames",
                    sample_rate, channels, wave_fmt.get_validbitspersample(), buffer_size
                );

                return Ok((audio_client, render_client, h_event, wave_fmt.clone(), buffer_size));
            }
            Err(e) => {
                let err_str = format!("{e}");
                // Handle AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED
                if err_str.contains("BUFFER_SIZE_NOT_ALIGNED") || err_str.contains("88890019") {
                    // Get aligned size and retry
                    if let Ok(aligned_size) = audio_client.get_buffer_size() {
                        let aligned_period = calculate_period_100ns(
                            aligned_size as i64,
                            sample_rate as i64,
                        );

                        drop(audio_client);
                        let mut audio_client2 = device
                            .get_iaudioclient()
                            .map_err(|e| format!("get_iaudioclient retry: {e}"))?;

                        let stream_mode2 = StreamMode::EventsExclusive {
                            period_hns: aligned_period,
                        };

                        if audio_client2
                            .initialize_client(&wave_fmt, &Direction::Render, &stream_mode2)
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

                            eprintln!(
                                "[WASAPI] Exclusive stream opened (aligned): {}Hz {}ch {}bit, buffer={}frames",
                                sample_rate, channels, wave_fmt.get_validbitspersample(), buffer_size
                            );

                            return Ok((audio_client2, render_client, h_event, wave_fmt.clone(), buffer_size));
                        }
                    }
                }
                eprintln!("[WASAPI] Format rejected: {e}");
                continue;
            }
        }
    }

    Err("no compatible exclusive format found".to_string())
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
            SampleType::Int => {
                match dst_store_bits {
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
                }
            }
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

fn render_thread(
    device_id: String,
    cmd_rx: mpsc::Receiver<ExclusiveCommand>,
    event_tx: mpsc::Sender<ExclusiveEvent>,
) {
    // COM must be initialized on this thread
    let hr = wasapi::initialize_mta();
    if hr.is_err() {
        let _ = event_tx.send(ExclusiveEvent::InitFailed(format!("COM init: {hr}")));
        return;
    }

    let (_enumerator, device) = match init_exclusive_client(&device_id) {
        Ok(v) => v,
        Err(e) => {
            let _ = event_tx.send(ExclusiveEvent::InitFailed(e));
            return;
        }
    };

    let mut state = RenderState::Idle;

    // Current stream resources (created on Load)
    let mut audio_client: Option<AudioClient> = None;
    let mut render_client: Option<AudioRenderClient> = None;
    let mut h_event: Option<Handle> = None;
    let mut wave_fmt: Option<WaveFormat> = None;
    let mut _buffer_size: u32 = 0;

    // Current PCM data
    let mut pcm_data: Vec<u8> = Vec::new();
    let mut pcm_sample_rate: u32 = 0;
    let mut pcm_channels: u32 = 0;
    let mut pcm_src_bps: u32 = 0;
    let mut pcm_duration: f64 = 0.0;
    let mut write_cursor: usize = 0;
    let mut frames_played: u64 = 0;
    let mut last_time_report = Instant::now();

    loop {
        match state {
            RenderState::Idle => {
                // Blocking wait for commands
                match cmd_rx.recv() {
                    Ok(ExclusiveCommand::Load {
                        pcm_data: data,
                        sample_rate,
                        channels,
                        bits_per_sample,
                        total_frames: _,
                        duration_secs,
                    }) => {
                        // Stop any existing stream
                        if let Some(ref ac) = audio_client {
                            let _ = ac.stop_stream();
                        }

                        match open_exclusive_stream(&device, sample_rate, channels, bits_per_sample) {
                            Ok((ac, rc, ev, wf, bs)) => {
                                pcm_data = data;
                                pcm_sample_rate = sample_rate;
                                pcm_channels = channels;
                                pcm_src_bps = bits_per_sample;
                                pcm_duration = duration_secs;
                                write_cursor = 0;
                                frames_played = 0;
                                _buffer_size = bs;
                                last_time_report = Instant::now();

                                audio_client = Some(ac);
                                render_client = Some(rc);
                                h_event = Some(ev);
                                wave_fmt = Some(wf);

                                let _ = event_tx.send(ExclusiveEvent::Duration(duration_secs));

                                // Start playback
                                if let Some(ref ac) = audio_client {
                                    let _ = ac.start_stream();
                                }
                                state = RenderState::Playing;
                                let _ = event_tx.send(ExclusiveEvent::StateChange("active".to_string()));
                            }
                            Err(e) => {
                                eprintln!("[WASAPI] Failed to open stream: {e}");
                                let _ = event_tx.send(ExclusiveEvent::InitFailed(e));
                                return;
                            }
                        }
                    }
                    Ok(ExclusiveCommand::Shutdown) | Err(_) => {
                        break;
                    }
                    _ => {} // Ignore other commands in idle
                }
            }

            RenderState::Playing => {
                // Non-blocking command check
                while let Ok(cmd) = cmd_rx.try_recv() {
                    match cmd {
                        ExclusiveCommand::Pause => {
                            if let Some(ref ac) = audio_client {
                                let _ = ac.stop_stream();
                            }
                            state = RenderState::Paused;
                            let _ = event_tx.send(ExclusiveEvent::StateChange("paused".to_string()));
                        }
                        ExclusiveCommand::Stop => {
                            if let Some(ref ac) = audio_client {
                                let _ = ac.stop_stream();
                            }
                            pcm_data.clear();
                            write_cursor = 0;
                            frames_played = 0;
                            state = RenderState::Idle;
                            let _ = event_tx.send(ExclusiveEvent::Stopped);
                        }
                        ExclusiveCommand::Seek(time) => {
                            if let Some(ref wf) = wave_fmt {
                                if let Some(ref ac) = audio_client {
                                    let _ = ac.stop_stream();
                                }

                                let src_bytes_per_sample = (pcm_src_bps / 8) as usize;
                                let src_bytes_per_frame = src_bytes_per_sample * pcm_channels as usize;

                                let target_frame = (time * pcm_sample_rate as f64) as u64;
                                write_cursor = (target_frame as usize) * src_bytes_per_frame;
                                if write_cursor > pcm_data.len() {
                                    write_cursor = pcm_data.len();
                                }
                                frames_played = target_frame;

                                if let Some(ref ac) = audio_client {
                                    let _ = ac.start_stream();
                                }
                            }
                        }
                        ExclusiveCommand::Load {
                            pcm_data: data,
                            sample_rate,
                            channels,
                            bits_per_sample,
                            total_frames: _,
                            duration_secs,
                        } => {
                            // Stop current, open new stream
                            if let Some(ref ac) = audio_client {
                                let _ = ac.stop_stream();
                            }

                            match open_exclusive_stream(&device, sample_rate, channels, bits_per_sample) {
                                Ok((ac, rc, ev, wf, bs)) => {
                                    pcm_data = data;
                                    pcm_sample_rate = sample_rate;
                                    pcm_channels = channels;
                                    pcm_src_bps = bits_per_sample;
                                    pcm_duration = duration_secs;
                                    write_cursor = 0;
                                    frames_played = 0;
                                    _buffer_size = bs;
                                    last_time_report = Instant::now();

                                    audio_client = Some(ac);
                                    render_client = Some(rc);
                                    h_event = Some(ev);
                                    wave_fmt = Some(wf);

                                    let _ = event_tx.send(ExclusiveEvent::Duration(duration_secs));
                                    let _ = audio_client.as_ref().unwrap().start_stream();
                                    let _ = event_tx.send(ExclusiveEvent::StateChange("active".to_string()));
                                }
                                Err(e) => {
                                    eprintln!("[WASAPI] Failed to open stream on Load: {e}");
                                    let _ = event_tx.send(ExclusiveEvent::InitFailed(e));
                                    return;
                                }
                            }
                        }
                        ExclusiveCommand::Shutdown => {
                            if let Some(ref ac) = audio_client {
                                let _ = ac.stop_stream();
                            }
                            return;
                        }
                        _ => {}
                    }
                }

                // If we transitioned out of Playing, skip the render step
                if !matches!(state, RenderState::Playing) {
                    continue;
                }

                // Wait for device buffer event
                if let Some(ref ev) = h_event {
                    let _ = ev.wait_for_event(50);
                }

                // Write PCM data to device
                if let (Some(ac), Some(rc), Some(wf)) =
                    (&audio_client, &render_client, &wave_fmt)
                {
                    let available = match ac.get_available_space_in_frames() {
                        Ok(n) => n as usize,
                        Err(_) => continue,
                    };

                    if available == 0 {
                        continue;
                    }

                    let src_bytes_per_sample = (pcm_src_bps / 8) as usize;
                    let src_bytes_per_frame = src_bytes_per_sample * pcm_channels as usize;
                    let dst_bytes_per_sample = wf.get_bitspersample() as usize / 8;
                    let dst_bytes_per_frame = dst_bytes_per_sample * pcm_channels as usize;

                    let remaining_src_bytes = pcm_data.len().saturating_sub(write_cursor);
                    let remaining_frames = if src_bytes_per_frame > 0 {
                        remaining_src_bytes / src_bytes_per_frame
                    } else {
                        0
                    };

                    if remaining_frames == 0 {
                        // Track finished — write silence and signal completion
                        let silence = vec![0u8; available * dst_bytes_per_frame];
                        let _ = rc.write_to_device(available, &silence, None);

                        let _ = event_tx.send(ExclusiveEvent::TimeUpdate(pcm_duration));
                        let _ = event_tx.send(ExclusiveEvent::StateChange("completed".to_string()));

                        if let Some(ref ac) = audio_client {
                            let _ = ac.stop_stream();
                        }
                        state = RenderState::Idle;
                        continue;
                    }

                    let frames_to_write = available.min(remaining_frames);
                    let src_chunk_size = frames_to_write * src_bytes_per_frame;
                    let src_chunk = &pcm_data[write_cursor..write_cursor + src_chunk_size];

                    // Convert if needed
                    let dst_store = wf.get_bitspersample() as u32;
                    let dst_valid = wf.get_validbitspersample() as u32;
                    let dst_type = wf.get_subformat().unwrap_or(SampleType::Int);

                    let write_data = if pcm_src_bps == dst_store && matches!(dst_type, SampleType::Int) {
                        // No conversion needed
                        src_chunk.to_vec()
                    } else {
                        convert_pcm_frame(
                            src_chunk,
                            pcm_src_bps,
                            dst_store,
                            dst_valid,
                            &dst_type,
                            pcm_channels,
                        )
                    };

                    if let Err(e) = rc.write_to_device(frames_to_write, &write_data, None) {
                        eprintln!("[WASAPI] write error: {e}");
                    }

                    write_cursor += src_chunk_size;
                    frames_played += frames_to_write as u64;

                    // Report time ~every 200ms
                    if last_time_report.elapsed().as_millis() >= 200 {
                        let pos = frames_played as f64 / pcm_sample_rate as f64;
                        let _ = event_tx.send(ExclusiveEvent::TimeUpdate(pos));
                        last_time_report = Instant::now();
                    }
                }
            }

            RenderState::Paused => {
                // Blocking wait for commands
                match cmd_rx.recv() {
                    Ok(ExclusiveCommand::Play) => {
                        if let Some(ref ac) = audio_client {
                            let _ = ac.start_stream();
                        }
                        state = RenderState::Playing;
                        let _ = event_tx.send(ExclusiveEvent::StateChange("active".to_string()));
                    }
                    Ok(ExclusiveCommand::Stop) => {
                        if let Some(ref ac) = audio_client {
                            let _ = ac.stop_stream();
                        }
                        pcm_data.clear();
                        write_cursor = 0;
                        frames_played = 0;
                        state = RenderState::Idle;
                        let _ = event_tx.send(ExclusiveEvent::Stopped);
                    }
                    Ok(ExclusiveCommand::Seek(time)) => {
                        if let Some(ref _wf) = wave_fmt {
                            let src_bytes_per_sample = (pcm_src_bps / 8) as usize;
                            let src_bytes_per_frame = src_bytes_per_sample * pcm_channels as usize;

                            let target_frame = (time * pcm_sample_rate as f64) as u64;
                            write_cursor = (target_frame as usize) * src_bytes_per_frame;
                            if write_cursor > pcm_data.len() {
                                write_cursor = pcm_data.len();
                            }
                            frames_played = target_frame;
                        }
                    }
                    Ok(ExclusiveCommand::Load {
                        pcm_data: data,
                        sample_rate,
                        channels,
                        bits_per_sample,
                        total_frames: _,
                        duration_secs,
                    }) => {
                        if let Some(ref ac) = audio_client {
                            let _ = ac.stop_stream();
                        }

                        match open_exclusive_stream(&device, sample_rate, channels, bits_per_sample) {
                            Ok((ac, rc, ev, wf, bs)) => {
                                pcm_data = data;
                                pcm_sample_rate = sample_rate;
                                pcm_channels = channels;
                                pcm_src_bps = bits_per_sample;
                                pcm_duration = duration_secs;
                                write_cursor = 0;
                                frames_played = 0;
                                _buffer_size = bs;
                                last_time_report = Instant::now();

                                audio_client = Some(ac);
                                render_client = Some(rc);
                                h_event = Some(ev);
                                wave_fmt = Some(wf);

                                let _ = event_tx.send(ExclusiveEvent::Duration(duration_secs));
                                let _ = audio_client.as_ref().unwrap().start_stream();
                                state = RenderState::Playing;
                                let _ = event_tx.send(ExclusiveEvent::StateChange("active".to_string()));
                            }
                            Err(e) => {
                                eprintln!("[WASAPI] Failed to open stream on Load: {e}");
                                let _ = event_tx.send(ExclusiveEvent::InitFailed(e));
                                return;
                            }
                        }
                    }
                    Ok(ExclusiveCommand::Shutdown) | Err(_) => {
                        if let Some(ref ac) = audio_client {
                            let _ = ac.stop_stream();
                        }
                        return;
                    }
                    _ => {}
                }
            }
        }
    }
}
