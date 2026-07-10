use std::io::{Read, Seek};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use symphonia::core::codecs::CodecParameters;
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::formats::FormatOptions;
use symphonia::core::formats::probe::Hint;
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::MetadataOptions;

use wasapi::{
    AudioClient, AudioRenderClient, DeviceEnumerator, Direction, Handle, SampleType, StreamMode,
    WaveFormat, calculate_period_100ns,
};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW,
};

use crate::player::declick::{RESYNC_SILENCE_MS, silence_frames};
use crate::player::throttle::{DECODE_AHEAD_SECS, throttle_decode_ahead};

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
        start_secs: f64,
        start_paused: bool,
    },
    PushPcm {
        stream_id: u32,
        pcm_data: Vec<u8>,
    },
    EndStream {
        stream_id: u32,
    },
    /// In-place seek applied by the live decoder: flush buffered PCM and
    /// re-base the reported position, without reopening the client.
    ResetForSeek {
        stream_id: u32,
        start_secs: f64,
    },
    /// Stream-scoped like PushPcm/EndStream (mirrors ASIO's Play/Pause): un-scoped, a
    /// premature Play (racing the decoder's probe) or a stale one from a superseded track
    /// resumed the OLD armed track's PCM until the new StartStream landed.
    Play {
        stream_id: u32,
    },
    Pause {
        stream_id: u32,
    },
    /// Drop the IAudioClient (free the endpoint for other apps) on a real stop,
    /// while keeping the render thread alive. The next StartStream reopens it.
    ReleaseDevice,
    Shutdown,
}

pub(super) enum ExclusiveEvent {
    TimeUpdate(f64),
    StateChange(super::PlaybackState),
    Duration(f64),
    InitFailed(String),
    DeviceLocked(String),
    /// The device can't do THIS track's rate/bit-depth in exclusive (e.g. 88.2/176.4/192k on a
    /// 96k-max DAC), but exclusive itself works for other rates. Per-track shared fallback,
    /// keeping exclusive enabled -- mirrors `AsioEvent::RateUnsupported`.
    FormatUnsupported,
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
    buffered: Arc<AtomicU64>,
    thread: Option<JoinHandle<()>>,
}

// ---------------------------------------------------------------------------
// FLAC -> PCM decoding (symphonia)
// ---------------------------------------------------------------------------

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

#[allow(clippy::too_many_arguments)]
pub(super) fn stream_flac_reader_to_wasapi<R>(
    reader: R,
    byte_len: u64,
    stream_id: u32,
    cmd_tx: mpsc::Sender<ExclusiveCommand>,
    cancel: Arc<AtomicBool>,
    seek_to: Option<f64>,
    start_paused: bool,
    seek_rx: mpsc::Receiver<f64>,
    buffered: Arc<AtomicU64>,
) -> Result<(), String>
where
    R: Read + Seek + Send + Sync + 'static,
{
    let source = Box::new(SizedMediaSource {
        inner: reader,
        byte_len,
    });
    let mss = MediaSourceStream::new(source, Default::default());

    crate::vprintln!("[WASAPI-DBG] flac reader: starting probe (byte_len={byte_len})");
    let mut hint = Hint::new();
    hint.with_extension("flac");

    let mut format_reader = symphonia::default::get_probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(|e| format!("probe failed: {e}"))?;

    let track = format_reader
        .tracks()
        .iter()
        .find(|t| matches!(&t.codec_params, Some(CodecParameters::Audio(_))))
        .ok_or("no audio track")?
        .clone();

    let codec_params = match &track.codec_params {
        Some(CodecParameters::Audio(p)) => p,
        _ => return Err("no audio track".to_string()),
    };
    let sample_rate = codec_params.sample_rate.ok_or("no sample rate")?;
    let channels = codec_params
        .channels
        .as_ref()
        .ok_or("no channel info")?
        .count() as u32;
    let bits_per_sample = codec_params.bits_per_sample.ok_or("no bits_per_sample")?;
    let n_frames = track.num_frames.unwrap_or(0);
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

    crate::vprintln!(
        "[WASAPI-DBG] flac reader: probe OK ({sample_rate}Hz {channels}ch {stored_bps}bit), sending StartStream"
    );
    // StartStream FIRST at offset 0 so the render adopts this stream_id and opens
    // the client immediately, rather than staying silent while a forward seek into
    // not-yet-downloaded data blocks below. The real landing position follows via
    // ResetForSeek (which needs the stream_id this sets).
    cmd_tx
        .send(ExclusiveCommand::StartStream {
            stream_id,
            sample_rate,
            channels,
            bits_per_sample: stored_bps,
            duration_secs,
            start_secs: 0.0,
            start_paused,
        })
        .map_err(|_| "failed to send StartStream".to_string())?;

    // Source seek AFTER StartStream so a forward seek past the buffered PCM moves
    // (mirrors do_decode_seek). On success re-base the render via ResetForSeek; on
    // failure (offset not yet downloaded) seed an initial seek the decode loop
    // retries, rather than playing from 0.
    let mut pending_initial_seek: Option<f64> = None;
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
                            start_secs: actual,
                        })
                        .is_err()
                    {
                        return Ok(());
                    }
                }
                Err(e) => {
                    crate::vprintln!("[WASAPI] decoder seek to {t:.1}s failed, will retry: {e}");
                    pending_initial_seek = Some(t);
                }
            }
        }
    }

    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(codec_params, &AudioDecoderOptions::default())
        .map_err(|e| format!("decoder creation failed: {e}"))?;

    // Back-pressure target in unplayed PCM bytes (ASIO's is in samples):
    // unthrottled, a cached source decodes the whole track (~880 MiB at 192kHz).
    let throttle_hi =
        sample_rate as u64 * channels as u64 * (stored_bps as u64 / 8) * DECODE_AHEAD_SECS;

    let mut sample_buf: Vec<i32> = Vec::new();
    let mut diag_logged = false;

    loop {
        if cancel.load(Relaxed) {
            return Ok(());
        }

        if throttle_decode_ahead(
            &buffered,
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
        if let Some(t) = pending_seek
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
                        pending_initial_seek = Some(t);
                    }
                }
            }
        }

        let packet = match format_reader.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) => {
                // Real EOF: arm completion, then PARK on the seek channel instead of
                // returning -- dropping seek_rx would kill live seeks for the rest of the
                // track (a cached source decodes far ahead of playback). ResetForSeek
                // un-ends the stream on a later seek, re-arming completion after it.
                if cancel.load(Relaxed) {
                    return Ok(());
                }
                crate::vprintln!("[WASAPI] decode: EOF (stream {stream_id}), parked for seeks");
                let _ = cmd_tx.send(ExclusiveCommand::EndStream { stream_id });
                // Disconnect is the ONLY exit while parked: every supersede path
                // stores `cancel` then drops the sender, and natural completion
                // just drops it -- either way nobody can seek this stream again.
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
                return Err(format!("decode packet error: {e}"));
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
}

// ---------------------------------------------------------------------------
// ExclusiveHandle - public API
// ---------------------------------------------------------------------------

impl ExclusiveHandle {
    /// Spawn the WASAPI render thread for the given device.
    /// `device_id` - wasapi device id string, or "default".
    pub fn spawn(device_id: String, gain: Arc<AtomicU32>) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel::<ExclusiveCommand>();
        let (event_tx, event_rx) = mpsc::channel::<ExclusiveEvent>();
        let buffered = Arc::new(AtomicU64::new(0));

        let render_buffered = buffered.clone();
        let handle = thread::spawn(move || {
            render_thread(device_id, cmd_rx, event_tx, gain, render_buffered);
        });

        Self {
            cmd_tx,
            event_rx,
            buffered,
            thread: Some(handle),
        }
    }

    pub fn send(&self, cmd: ExclusiveCommand) {
        let _ = self.cmd_tx.send(cmd);
    }

    pub fn command_sender(&self) -> mpsc::Sender<ExclusiveCommand> {
        self.cmd_tx.clone()
    }

    /// The shared unplayed-PCM byte counter, for the decode thread's back-pressure.
    pub fn buffered(&self) -> Arc<AtomicU64> {
        self.buffered.clone()
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

    // 20ms exclusive period: the 10ms minimum leaves no scheduling slack (one
    // late wake underruns), and music tolerates the extra latency. The
    // BUFFER_SIZE_NOT_ALIGNED retry below realigns if the driver needs it.
    const EXCLUSIVE_PERIOD_MS: i64 = 20;
    let period = calculate_period_100ns(
        sr as i64 * EXCLUSIVE_PERIOD_MS / 1000, // frames per period
        sr as i64,
    );

    candidates.into_iter().map(|fmt| (fmt, period)).collect()
}

fn init_exclusive_client(device_id: &str) -> Result<(DeviceEnumerator, wasapi::Device), String> {
    let enumerator = DeviceEnumerator::new().map_err(|e| format!("DeviceEnumerator: {e}"))?;

    // "auto"/"default"/empty are the default-device sentinels (TIDAL's selectSystemDevice
    // and the sticky exclusive re-assert send "auto", matching the shared cpal path); resolve
    // them to the OS default render endpoint instead of a literal device-name lookup.
    let device = if device_id == "default" || device_id == "auto" || device_id.is_empty() {
        enumerator
            .get_default_device(&Direction::Render)
            .map_err(|e| format!("default device: {e}"))?
    } else {
        // `device_id` is cpal's DeviceDesc ("Speakers"), NOT the endpoint id nor
        // the FriendlyName. Match get_description() (same property cpal stores) to
        // line up with the shared path's find_output_device; fall back to
        // FriendlyName for the rare driver where cpal itself did.
        let collection = enumerator
            .get_device_collection(&Direction::Render)
            .map_err(|e| format!("device collection: {e}"))?;
        let count = collection
            .get_nbr_devices()
            .map_err(|e| format!("device count: {e}"))?;
        (0..count)
            .find_map(|i| {
                let dev = collection.get_device_at_index(i).ok()?;
                let matches = dev.get_description().ok().as_deref() == Some(device_id)
                    || dev.get_friendlyname().ok().as_deref() == Some(device_id);
                matches.then_some(dev)
            })
            .ok_or_else(|| format!("device '{device_id}': not found"))?
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
                // Propagate a transient lock instead of the generic error, so
                // try_open_stream emits DeviceLocked, not the permanent
                // ExclusiveModeNotAllowed that demotes the saved mode.
                if is_device_in_use_error(&err_str) {
                    return Err(err_str);
                }
                // Exclusive disabled for the device in Windows: every candidate
                // returns the same error, so stop probing and propagate it (lands
                // in InitFailed -> ExclusiveModeNotAllowed, the permanent class).
                if is_exclusive_mode_disabled_error(&err_str) {
                    crate::vprintln!(
                        "[WASAPI] Exclusive mode disabled for this device in Windows \
                         (Sound > device > Advanced > Allow exclusive control): {e}"
                    );
                    return Err(err_str);
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

/// Check if WASAPI reports exclusive mode disabled system-wide (the Windows
/// "Allow applications to take exclusive control of this device" toggle is OFF).
/// This is a session-level block (AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED,
/// 0x8889000E), not a format-level rejection, so no other candidate can succeed.
fn is_exclusive_mode_disabled_error(err_str: &str) -> bool {
    err_str.contains("EXCLUSIVE_MODE_NOT_ALLOWED")
        || err_str.contains("8889000e")
        || err_str.contains("8889000E")
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
    gain: f32,
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

        // Digital gain (<1.0 attenuates; 1.0 leaves the sample untouched).
        let sample_i32 = if gain < 1.0 {
            ((sample_i32 as f32) * gain) as i32
        } else {
            sample_i32
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

/// Absolute frame index a seek/resume to `secs` corresponds to. Used as the
/// `frames_played` baseline so position reporting stays correct after a seek.
fn position_frames(secs: f64, sample_rate: u32) -> u64 {
    if sample_rate > 0 && secs.is_finite() && secs > 0.0 {
        (secs * sample_rate as f64) as u64
    } else {
        0
    }
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
            } else if e.as_str() == "no compatible exclusive format found" {
                // Per-track: this track's format isn't exclusive-compatible, but exclusive
                // itself is fine for other rates. Play shared, keep exclusive on (do NOT treat
                // it as a device-wide ExclusiveModeNotAllowed, which would disable exclusive).
                let _ = event_tx.send(ExclusiveEvent::FormatUnsupported);
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
    // Unplayed bytes (pcm_data.len() - write_cursor); republished at every
    // push/consume/clear for the decode thread's throttle.
    buffered: Arc<AtomicU64>,
    current_stream_id: Option<u32>,
    // Pre-adoption transport intent (stream_id, play): a Play/Pause for a stream
    // whose probe-delayed StartStream hasn't arrived yet (FIFO makes this
    // deterministic). Every adoption consumes it: applied on id match, discarded
    // otherwise (ids never repeat).
    pending_transport: Option<(u32, bool)>,
    stream_ended: bool,
    last_time_report: Instant,
    state: RenderState,
    // Playing but the audio client is not started yet: held until a PCM cushion
    // is buffered so a seeked start does not underrun into silence (Playing loop).
    pending_start: bool,
    // Whether the WASAPI clock is running. Distinct from pending_start: a fresh
    // start defers start_stream until a buffer of real PCM is pre-filled, and a
    // mid-stream underrun re-arms pending_start with the clock still running.
    client_started: bool,
    // Frames of post-format-change resync silence the render loop still owes the
    // endpoint before real PCM, to mask the DAC PLL relock at the new rate.
    post_start_silence_remaining: u32,
}

impl RenderContext {
    fn new(buffered: Arc<AtomicU64>) -> Self {
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
            buffered,
            current_stream_id: None,
            pending_transport: None,
            stream_ended: true,
            last_time_report: Instant::now(),
            state: RenderState::Idle,
            pending_start: false,
            client_started: false,
            post_start_silence_remaining: 0,
        }
    }

    fn stop_audio_client(&self) {
        if let Some(ref ac) = self.audio_client {
            let _ = ac.stop_stream();
        }
    }

    fn publish_buffered(&self) {
        self.buffered.store(
            self.pcm_data.len().saturating_sub(self.write_cursor) as u64,
            Relaxed,
        );
    }

    /// Release the exclusive device on a real stop: drop the IAudioClient so other
    /// apps regain the endpoint, then park in Idle. The next StartStream reopens it
    /// (handle_start_stream sees `audio_client == None` and opens a fresh client).
    fn release_device(&mut self) {
        self.stop_audio_client();
        self.render_client = None;
        self.h_event = None;
        self.audio_client = None;
        self.pcm_data.clear();
        self.write_cursor = 0;
        self.publish_buffered();
        self.frames_played = 0;
        self.current_stream_id = None;
        self.pending_transport = None;
        self.stream_ended = true;
        self.client_started = false;
        self.pending_start = false;
        self.post_start_silence_remaining = 0;
        self.state = RenderState::Idle;
    }

    /// Open a new stream, reset all PCM/playback state, start playback.
    /// Postconditions on Ok: pcm_data cleared, cursors zeroed, stream_id set,
    /// audio resources replaced, state = Playing, Duration + Active events sent.
    /// On Err: an error event was sent and render_thread must exit.
    #[allow(clippy::too_many_arguments)]
    fn handle_start_stream(
        &mut self,
        device: &wasapi::Device,
        event_tx: &mpsc::Sender<ExclusiveEvent>,
        stream_id: u32,
        sample_rate: u32,
        channels: u32,
        bits_per_sample: u32,
        duration_secs: f64,
        start_secs: f64,
        start_paused: bool,
    ) -> Result<(), ()> {
        // Reuse the open client on an unchanged format (reopening pops the DAC).
        // A format change needs a fresh one: an exclusive client owns the device,
        // so release the old one first (else the new initialize fails with
        // AUDCLNT_E_DEVICE_IN_USE, 0x8889000A).
        let same_format = self.audio_client.is_some()
            && self.pcm_sample_rate == sample_rate
            && self.pcm_channels == channels
            && self.pcm_src_bps == bits_per_sample;
        if !same_format {
            // A client was already playing iff this is an actual rate change (not the
            // first open or a reopen after release): only then is there a DAC relock to
            // mask with post-start silence.
            let had_client = self.audio_client.is_some();
            self.stop_audio_client();
            self.render_client = None;
            self.h_event = None;
            self.audio_client = None;

            let (ac, rc, ev, wf, bs) = try_open_stream(
                device,
                sample_rate,
                channels,
                bits_per_sample,
                &self.audio_client,
                event_tx,
            )?;

            self.audio_client = Some(ac);
            self.render_client = Some(rc);
            self.h_event = Some(ev);
            self.wave_fmt = Some(wf);
            self.buffer_size = bs;
            // Mask the DAC PLL relock at the new rate with a resync silence (shared sizing
            // with the ASIO backend).
            self.post_start_silence_remaining = if had_client {
                silence_frames(sample_rate, RESYNC_SILENCE_MS) as u32
            } else {
                0
            };
        } else if let Some(ref ac) = self.audio_client {
            // Reusing the open client: stop + Reset() to flush stale frames from
            // the endpoint. Reset needs the stream stopped; the start branch below
            // restarts it.
            let _ = ac.stop_stream();
            let _ = ac.reset_stream();
            // Same-format reuse: continuous endpoint, no relock -> no resync silence
            // (clear any leftover from a just-prior rate change).
            self.post_start_silence_remaining = 0;
        }

        self.pcm_data.clear();
        self.pcm_sample_rate = sample_rate;
        self.pcm_channels = channels;
        self.pcm_src_bps = bits_per_sample;
        self.pcm_duration = duration_secs;
        self.write_cursor = 0;
        self.publish_buffered();
        self.frames_played = position_frames(start_secs, sample_rate);
        self.current_stream_id = Some(stream_id);
        self.stream_ended = false;
        self.last_time_report = Instant::now();
        // Apply a transport command that raced ahead of this adoption (the player
        // can send Play/Pause for this stream before its probe-delayed StartStream
        // lands). Consumed on EVERY adoption: applied on id match, else discarded.
        let start_paused = match self.pending_transport.take() {
            Some((id, play)) if id == stream_id => !play,
            _ => start_paused,
        };

        let _ = event_tx.send(ExclusiveEvent::Duration(duration_secs));
        // Do NOT start the clock yet: per IAudioClient::Start the buffer must
        // hold real PCM first, else the tiny exclusive buffer underruns at the
        // silence->audio edge (saturation). The Playing loop pre-fills, then starts.
        self.client_started = false;
        if start_paused {
            // A paused load has an empty endpoint, so arm the deferred start: Play
            // pre-fills before starting, rather than start_stream() on empty.
            self.pending_start = true;
            self.stop_audio_client();
            self.state = RenderState::Paused;
            let _ = event_tx.send(ExclusiveEvent::StateChange(super::PlaybackState::Paused));
        } else {
            self.pending_start = true;
            self.state = RenderState::Playing;
            let _ = event_tx.send(ExclusiveEvent::StateChange(super::PlaybackState::Active));
        }
        Ok(())
    }

    fn handle_push_pcm(&mut self, stream_id: u32, data: Vec<u8>) {
        if self.current_stream_id == Some(stream_id) {
            // Reclaim the played prefix before appending: with the decode
            // throttle this bounds pcm_data to ~2s of unplayed PCM.
            if self.write_cursor > 0 {
                self.pcm_data.drain(..self.write_cursor);
                self.write_cursor = 0;
            }
            self.pcm_data.extend_from_slice(&data);
            self.publish_buffered();
        }
    }

    fn handle_end_stream(&mut self, stream_id: u32) {
        if self.current_stream_id == Some(stream_id) {
            self.stream_ended = true;
        }
    }

    fn handle_reset_for_seek(&mut self, stream_id: u32, start_secs: f64) {
        if self.current_stream_id != Some(stream_id) {
            return;
        }
        // Always stop+Reset() the endpoint (a no-op on a stopped/unstarted
        // client) to drop queued pre-seek/pre-pause frames, then defer the
        // restart so the loop pre-fills real PCM before starting -- an unfilled
        // exclusive buffer here is what made seeks saturate.
        if let Some(ref ac) = self.audio_client {
            let _ = ac.stop_stream();
            let _ = ac.reset_stream();
        }
        self.client_started = false;
        self.pcm_data.clear();
        self.write_cursor = 0;
        self.publish_buffered();
        self.frames_played = position_frames(start_secs, self.pcm_sample_rate);
        self.stream_ended = false;
        self.pending_start = true;
        self.last_time_report = Instant::now();
    }
}

/// RAII guard registering the render thread with the MMCSS "Pro Audio" task so it
/// wakes on the ~10ms device cadence. The default ~15ms timer wakes too late and
/// starves the endpoint, which glitches. Reverted on drop.
struct ProAudioMmcss(HANDLE);

impl ProAudioMmcss {
    fn register() -> Option<Self> {
        let mut task_index = 0u32;
        // SAFETY: `task_index` is a valid out-pointer; the returned handle is
        // owned by this guard and released in `Drop`.
        match unsafe {
            AvSetMmThreadCharacteristicsW(windows::core::w!("Pro Audio"), &mut task_index)
        } {
            Ok(handle) => Some(Self(handle)),
            Err(e) => {
                crate::vprintln!("[WASAPI] MMCSS 'Pro Audio' registration failed: {e}");
                None
            }
        }
    }
}

impl Drop for ProAudioMmcss {
    fn drop(&mut self) {
        // SAFETY: `self.0` is a live handle returned by AvSetMmThreadCharacteristicsW.
        unsafe {
            let _ = AvRevertMmThreadCharacteristics(self.0);
        }
    }
}

fn render_thread(
    device_id: String,
    cmd_rx: mpsc::Receiver<ExclusiveCommand>,
    event_tx: mpsc::Sender<ExclusiveEvent>,
    gain: Arc<AtomicU32>,
    buffered: Arc<AtomicU64>,
) {
    let _ = render_thread_inner(device_id, cmd_rx, event_tx, gain, buffered);
}

fn render_thread_inner(
    device_id: String,
    cmd_rx: mpsc::Receiver<ExclusiveCommand>,
    event_tx: mpsc::Sender<ExclusiveEvent>,
    gain: Arc<AtomicU32>,
    buffered: Arc<AtomicU64>,
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
    crate::vprintln!("[WASAPI-DBG] render: device '{device_id}' resolved, awaiting StartStream");

    // MMCSS "Pro Audio": wake on the device period, not the ~15ms timer. Held
    // for the render loop (reverted on drop).
    let _mmcss = ProAudioMmcss::register();

    let mut ctx = RenderContext::new(buffered);
    let mut diag_logged = false;
    // Startup render-loop diagnostic (LOGS=3): classify each of the first 150
    // Playing iterations to confirm/refute a buffer underrun at startup. Cheap
    // per-iteration counters, a single dump at the end (low observer effect).
    let mut diag_it = 0u32;
    let mut diag_done = false;
    let mut diag_last = Instant::now();
    let mut diag_max_gap = 0u128;
    let mut diag_late = 0u32;
    let (mut diag_s, mut diag_u, mut diag_p, mut diag_f) = (0u32, 0u32, 0u32, 0u32);
    let mut diag_min_rem = usize::MAX;

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
                        start_secs,
                        start_paused,
                    }) => {
                        crate::vprintln!(
                            "[WASAPI-DBG] render: StartStream received, opening exclusive client"
                        );
                        ctx.handle_start_stream(
                            &device,
                            &event_tx,
                            stream_id,
                            sample_rate,
                            channels,
                            bits_per_sample,
                            duration_secs,
                            start_secs,
                            start_paused,
                        )?;
                    }
                    Ok(ExclusiveCommand::ReleaseDevice) => ctx.release_device(),
                    Ok(ExclusiveCommand::Shutdown) | Err(_) => break,
                    _ => {} // Ignore other commands in idle
                }
            }

            RenderState::Playing => {
                while let Ok(cmd) = cmd_rx.try_recv() {
                    match cmd {
                        ExclusiveCommand::Play { stream_id } => {
                            // Play for the live stream while already Playing is a no-op;
                            // one for a not-yet-adopted stream is latched so its
                            // probe-delayed StartStream applies it.
                            if ctx.current_stream_id != Some(stream_id) {
                                ctx.pending_transport = Some((stream_id, true));
                            }
                        }
                        ExclusiveCommand::Pause { stream_id } => {
                            // Stream-scoped: a stale pause from a superseded track must
                            // not stop the live stream; one for a not-yet-adopted stream
                            // is latched so its StartStream starts paused.
                            if ctx.current_stream_id != Some(stream_id) {
                                ctx.pending_transport = Some((stream_id, false));
                                continue;
                            }
                            // Stop + Reset() flush the stale endpoint tail; pending_start
                            // makes resume re-prime a full period before Start(). A bare
                            // restart over the stale tail saturates (the exclusive buffer
                            // must hold real PCM before Start). Like handle_reset_for_seek,
                            // minus the PCM/cursor reset since resume continues in place.
                            if let Some(ref ac) = ctx.audio_client {
                                let _ = ac.stop_stream();
                                let _ = ac.reset_stream();
                            }
                            ctx.client_started = false;
                            ctx.pending_start = true;
                            ctx.state = RenderState::Paused;
                            let _ = event_tx
                                .send(ExclusiveEvent::StateChange(super::PlaybackState::Paused));
                        }
                        ExclusiveCommand::StartStream {
                            stream_id,
                            sample_rate,
                            channels,
                            bits_per_sample,
                            duration_secs,
                            start_secs,
                            start_paused,
                        } => {
                            ctx.handle_start_stream(
                                &device,
                                &event_tx,
                                stream_id,
                                sample_rate,
                                channels,
                                bits_per_sample,
                                duration_secs,
                                start_secs,
                                start_paused,
                            )?;
                        }
                        ExclusiveCommand::PushPcm {
                            stream_id,
                            pcm_data: data,
                        } => ctx.handle_push_pcm(stream_id, data),
                        ExclusiveCommand::EndStream { stream_id } => {
                            ctx.handle_end_stream(stream_id)
                        }
                        ExclusiveCommand::ResetForSeek {
                            stream_id,
                            start_secs,
                        } => ctx.handle_reset_for_seek(stream_id, start_secs),
                        ExclusiveCommand::ReleaseDevice => ctx.release_device(),
                        ExclusiveCommand::Shutdown => {
                            ctx.stop_audio_client();
                            return Ok(());
                        }
                    }
                }

                if !matches!(ctx.state, RenderState::Playing) {
                    continue;
                }

                // Before the clock starts the event never fires; poll briefly to
                // build the cushion. Once running it fires each period (50ms cap).
                if let Some(ref ev) = ctx.h_event {
                    let timeout_ms = if ctx.client_started { 50 } else { 5 };
                    let _ = ev.wait_for_event(timeout_ms);
                }

                let mut start_clock_now = false;
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

                    // Post-format-change resync silence: hold the endpoint at zero while the
                    // DAC PLL relocks at the new rate. Emits silence without consuming pcm_data,
                    // so the track head is delayed, not dropped.
                    if ctx.post_start_silence_remaining > 0 {
                        let silence = vec![0u8; available * dst_bytes_per_frame];
                        let _ = rc.write_to_device(available, &silence, None);
                        ctx.post_start_silence_remaining = ctx
                            .post_start_silence_remaining
                            .saturating_sub(available as u32);
                        if !ctx.client_started {
                            // Pre-roll is in the endpoint; start the clock so it plays out and
                            // frees space for the next silence buffer (exclusive event mode
                            // needs a buffer written before Start()).
                            let _ = ac.start_stream();
                            ctx.client_started = true;
                        }
                        continue;
                    }

                    let remaining_src_bytes = ctx.pcm_data.len().saturating_sub(ctx.write_cursor);
                    let remaining_frames = remaining_src_bytes
                        .checked_div(src_bytes_per_frame)
                        .unwrap_or(0);

                    // Cushion: hold the deferred start until ~0.5s is buffered (or
                    // the stream ends). A fresh start just accumulates; a mid-stream
                    // underrun feeds the still-running client silence below.
                    let buffering = ctx.pending_start
                        && !ctx.stream_ended
                        && remaining_frames < ctx.pcm_sample_rate as usize / 2;
                    if ctx.pending_start && !buffering {
                        ctx.pending_start = false;
                    }

                    if !diag_done && ctx.client_started {
                        let gap = diag_last.elapsed().as_millis();
                        diag_last = Instant::now();
                        if diag_it > 0 {
                            diag_max_gap = diag_max_gap.max(gap);
                            // Late = >1.5x the ~20ms device period (a wake that
                            // risks an endpoint underrun). Scale this with the
                            // period in negotiate_format if it changes.
                            if gap > 30 {
                                diag_late += 1;
                            }
                        }
                        if buffering {
                            diag_s += 1;
                        } else if remaining_frames == 0 {
                            diag_u += 1;
                        } else if remaining_frames < available {
                            diag_p += 1;
                            diag_min_rem = diag_min_rem.min(remaining_frames);
                        } else {
                            diag_f += 1;
                            diag_min_rem = diag_min_rem.min(remaining_frames);
                        }
                        diag_it += 1;
                        if diag_it >= 150 {
                            diag_done = true;
                            crate::vprintln3!(
                                "[WASAPI-DIAG] startup {diag_it}it: silence={diag_s} underrun_empty={diag_u} underrun_partial={diag_p} full={diag_f} max_gap={diag_max_gap}ms late(>30ms)={diag_late} min_rem_frames={diag_min_rem}"
                            );
                        }
                    }

                    // Clock not running yet: accumulate the cushion without writing
                    // to the endpoint; the real-write path below pre-fills + starts.
                    if !ctx.client_started && (buffering || remaining_frames == 0) {
                        if ctx.stream_ended && remaining_frames == 0 {
                            // Stream ended before any audio decoded: complete now.
                            let _ = event_tx.send(ExclusiveEvent::TimeUpdate(ctx.pcm_duration));
                            let _ = event_tx
                                .send(ExclusiveEvent::StateChange(super::PlaybackState::Completed));
                            ctx.stop_audio_client();
                            ctx.current_stream_id = None;
                            ctx.stream_ended = true;
                            ctx.state = RenderState::Idle;
                        }
                        continue;
                    }

                    if remaining_frames == 0 || buffering {
                        // A mid-stream underrun (no PCM, stream not ended): re-arm
                        // the start cushion so the next refill rebuilds a buffer
                        // before resuming, preventing a repeated stutter.
                        if remaining_frames == 0 && !ctx.stream_ended {
                            ctx.pending_start = true;
                        }
                        let silence = vec![0u8; available * dst_bytes_per_frame];
                        let _ = rc.write_to_device(available, &silence, None);

                        if buffering || !ctx.stream_ended {
                            continue;
                        }

                        let _ = event_tx.send(ExclusiveEvent::TimeUpdate(ctx.pcm_duration));
                        let _ = event_tx
                            .send(ExclusiveEvent::StateChange(super::PlaybackState::Completed));

                        ctx.stop_audio_client();
                        ctx.client_started = false;
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
                    // Live digital gain (exclusive bypasses the OS mixer). At 1.0
                    // the passthrough fast path stays bit-perfect.
                    let g = f32::from_bits(gain.load(Relaxed));

                    let write_data: std::borrow::Cow<'_, [u8]> = if ctx.pcm_src_bps == dst_store
                        && matches!(dst_type, SampleType::Int)
                        && g >= 1.0
                    {
                        std::borrow::Cow::Borrowed(src_chunk)
                    } else {
                        std::borrow::Cow::Owned(convert_pcm_frame(
                            src_chunk,
                            ctx.pcm_src_bps,
                            dst_store,
                            dst_valid,
                            &dst_type,
                            ctx.pcm_channels,
                            g,
                        ))
                    };

                    if !diag_logged {
                        diag_logged = true;
                        let (mut mn, mut mx) = (i16::MAX, i16::MIN);
                        if dst_store == 16 {
                            for s in write_data.chunks_exact(2) {
                                let v = i16::from_le_bytes([s[0], s[1]]);
                                mn = mn.min(v);
                                mx = mx.max(v);
                            }
                        }
                        let head: Vec<String> = write_data
                            .iter()
                            .take(8)
                            .map(|b| format!("{b:02x}"))
                            .collect();
                        crate::vprintln3!(
                            "[WASAPI-DIAG] write src_bps={} dst_store={} dst_valid={} int={} gain={:.3} i16[{mn} {mx}] bytes=[{}]",
                            ctx.pcm_src_bps,
                            dst_store,
                            dst_valid,
                            matches!(dst_type, SampleType::Int),
                            g,
                            head.join(" "),
                        );
                    }

                    // Exclusive event mode requires full-buffer packets: pad a
                    // short tail (track end) with silence, else the write returns
                    // AUDCLNT_E_BUFFER_SIZE_ERROR and drops the last frames.
                    let full_bytes = available * dst_bytes_per_frame;
                    let write_buf: std::borrow::Cow<'_, [u8]> = if write_data.len() < full_bytes {
                        let mut padded = vec![0u8; full_bytes];
                        padded[..write_data.len()].copy_from_slice(&write_data);
                        std::borrow::Cow::Owned(padded)
                    } else {
                        write_data
                    };

                    if let Err(e) = rc.write_to_device(available, &write_buf, None) {
                        crate::vprintln!("[WASAPI] write error: {e}");
                    }

                    // Advance by the real frames only; the silence padding is not
                    // part of the decoded stream or the playback position.
                    ctx.write_cursor += src_chunk_size;
                    ctx.frames_played += frames_to_write as u64;
                    ctx.publish_buffered();

                    if !ctx.client_started {
                        // First real buffer is now loaded -> start the clock once
                        // the audio-client borrow is released (after this if-let).
                        start_clock_now = true;
                    }

                    if ctx.last_time_report.elapsed().as_millis() >= 200 {
                        // Clamp to frames actually backed by buffered PCM so a
                        // forward seek into a still-downloading region never reports
                        // a position ahead of audible audio.
                        let consumed_frames = ctx
                            .write_cursor
                            .checked_div(src_bytes_per_frame)
                            .unwrap_or(0) as u64;
                        let buffered_frames = ctx
                            .pcm_data
                            .len()
                            .checked_div(src_bytes_per_frame)
                            .unwrap_or(0) as u64;
                        let baseline = ctx.frames_played.saturating_sub(consumed_frames);
                        let pos = ctx.frames_played.min(baseline + buffered_frames) as f64
                            / ctx.pcm_sample_rate as f64;
                        let _ = event_tx.send(ExclusiveEvent::TimeUpdate(pos));
                        ctx.last_time_report = Instant::now();
                    }
                }

                if start_clock_now {
                    // The endpoint is pre-filled, so start the clock. Reset the diag
                    // timer so the first gap reflects steady-state, not the cushion poll.
                    if let Some(ref ac) = ctx.audio_client {
                        let _ = ac.start_stream();
                    }
                    ctx.client_started = true;
                    diag_last = Instant::now();
                }
            }

            RenderState::Paused => match cmd_rx.recv() {
                Ok(ExclusiveCommand::Play { stream_id }) => {
                    // Stream-scoped: a premature Play for a not-yet-adopted stream
                    // (its StartStream lands only after the decoder's probe) or a
                    // stale one for a superseded stream must not resume the OLD
                    // armed context. Latch it: the adoption applies it on id match.
                    if ctx.current_stream_id != Some(stream_id) {
                        ctx.pending_transport = Some((stream_id, true));
                        continue;
                    }
                    // Pause armed pending_start + Reset() the endpoint, so the Playing
                    // loop re-primes a full period before Start() (first-start/seek path).
                    ctx.state = RenderState::Playing;
                    let _ =
                        event_tx.send(ExclusiveEvent::StateChange(super::PlaybackState::Active));
                }
                Ok(ExclusiveCommand::Pause { stream_id }) => {
                    // Already paused; only a pause for a not-yet-adopted stream
                    // matters -- latch it so its StartStream starts paused.
                    if ctx.current_stream_id != Some(stream_id) {
                        ctx.pending_transport = Some((stream_id, false));
                    }
                }
                Ok(ExclusiveCommand::StartStream {
                    stream_id,
                    sample_rate,
                    channels,
                    bits_per_sample,
                    duration_secs,
                    start_secs,
                    start_paused,
                }) => {
                    ctx.handle_start_stream(
                        &device,
                        &event_tx,
                        stream_id,
                        sample_rate,
                        channels,
                        bits_per_sample,
                        duration_secs,
                        start_secs,
                        start_paused,
                    )?;
                }
                Ok(ExclusiveCommand::PushPcm {
                    stream_id,
                    pcm_data: data,
                }) => ctx.handle_push_pcm(stream_id, data),
                Ok(ExclusiveCommand::EndStream { stream_id }) => ctx.handle_end_stream(stream_id),
                Ok(ExclusiveCommand::ResetForSeek {
                    stream_id,
                    start_secs,
                }) => ctx.handle_reset_for_seek(stream_id, start_secs),
                Ok(ExclusiveCommand::ReleaseDevice) => ctx.release_device(),
                Ok(ExclusiveCommand::Shutdown) | Err(_) => {
                    ctx.stop_audio_client();
                    return Ok(());
                }
            },
        }
    }

    Ok(())
}

#[cfg(test)]
mod position_frames_tests {
    use super::position_frames;

    #[test]
    fn maps_seconds_to_frames() {
        assert_eq!(position_frames(62.4, 44100), 2_751_840);
        assert_eq!(position_frames(0.0, 44100), 0);
        assert_eq!(position_frames(-1.0, 44100), 0);
        assert_eq!(position_frames(10.0, 0), 0);
        assert_eq!(position_frames(f64::NAN, 44100), 0);
    }
}

#[cfg(test)]
mod pcm_compaction_tests {
    use super::*;

    #[test]
    fn push_reclaims_played_prefix_and_publishes_unplayed() {
        let buffered = Arc::new(AtomicU64::new(0));
        let mut ctx = RenderContext::new(buffered.clone());
        ctx.current_stream_id = Some(7);

        ctx.handle_push_pcm(7, vec![1u8; 1000]);
        assert_eq!(ctx.pcm_data.len(), 1000);
        assert_eq!(buffered.load(Relaxed), 1000);

        // Simulate the render loop consuming 600 bytes.
        ctx.write_cursor = 600;
        ctx.handle_push_pcm(7, vec![2u8; 500]);
        // Played prefix reclaimed: only the 400 unplayed + 500 new bytes remain.
        assert_eq!(ctx.write_cursor, 0);
        assert_eq!(ctx.pcm_data.len(), 900);
        assert_eq!(ctx.pcm_data[0], 1);
        assert_eq!(ctx.pcm_data[400], 2);
        assert_eq!(buffered.load(Relaxed), 900);

        // A stale stream's push is dropped and publishes nothing.
        ctx.handle_push_pcm(8, vec![3u8; 100]);
        assert_eq!(ctx.pcm_data.len(), 900);
        assert_eq!(buffered.load(Relaxed), 900);
    }
}
