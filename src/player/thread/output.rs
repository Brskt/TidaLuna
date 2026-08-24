use cpal::traits::{DeviceTrait, HostTrait};
use rubato::Resampler;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering::Relaxed};

use crate::player::AudioDevice;

/// `cpal_stream_error` signal codes: the cpal error callback (audio thread) stores one,
/// `poll_playback` (player thread) reads it. `DEVICE_LOST` triggers a pipeline rebuild.
pub(super) const STREAM_ERR_NONE: u8 = 0;
pub(super) const STREAM_ERR_DEVICE_LOST: u8 = 1;
pub(super) const STREAM_ERR_UNKNOWN: u8 = 2;

// --- Helpers ---

pub(super) fn format_sample_rate(rate: u32) -> String {
    if rate.is_multiple_of(1000) {
        format!("{} kHz", rate / 1000)
    } else {
        format!("{:.1} kHz", rate as f64 / 1000.0)
    }
}

pub(super) fn format_duration_mmss(secs: f64) -> String {
    let total = secs as u32;
    format!("{}:{:02}", total / 60, total % 60)
}

pub(super) fn codec_name(codec: symphonia::core::codecs::audio::AudioCodecId) -> &'static str {
    use symphonia::core::codecs::audio::well_known::*;
    match codec {
        CODEC_ID_FLAC => "flac",
        CODEC_ID_AAC => "aac",
        CODEC_ID_MP3 => "mp3",
        CODEC_ID_VORBIS => "vorbis",
        CODEC_ID_OPUS => "opus",
        CODEC_ID_PCM_S16LE | CODEC_ID_PCM_S24LE | CODEC_ID_PCM_S32LE | CODEC_ID_PCM_F32LE => "pcm",
        CODEC_ID_ALAC => "alac",
        _ => "unknown",
    }
}

pub(super) fn enumerate_audio_devices() -> Vec<AudioDevice> {
    let host = cpal::default_host();
    let mut devices = vec![AudioDevice {
        controllable_volume: true,
        id: "default".to_string(),
        name: "System Default".to_string(),
        r#type: Some("systemDefault".to_string()),
    }];

    if let Ok(output_devices) = host.output_devices() {
        for device in output_devices {
            if let Ok(desc) = device.description() {
                let name = desc.name().to_string();
                devices.push(AudioDevice {
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

/// True when `id` selects the OS default device rather than a specific one:
/// `"default"` (the device-list entry) and `"auto"` (selectSystemDevice) are aliases.
pub(super) fn is_default_selector(id: &str) -> bool {
    id == "default" || id == "auto"
}

pub(super) fn find_output_device(device_id: &str) -> Option<cpal::Device> {
    let host = cpal::default_host();
    // A default selector maps to the OS default without enumerating.
    if is_default_selector(device_id) {
        return host.default_output_device();
    }

    host.output_devices().ok()?.find(|d| {
        d.description()
            .ok()
            .map(|desc| desc.name() == device_id)
            .unwrap_or(false)
    })
}

pub(super) fn output_device_name(device: &cpal::Device) -> Option<String> {
    device.description().ok().map(|d| d.name().to_string())
}

// Resolve a requested id to a concrete device, falling back to the OS default
// when the id is None or names a device that isn't present.
pub(super) fn resolve_device(device_id: Option<&str>) -> Option<cpal::Device> {
    device_id
        .and_then(find_output_device)
        .or_else(|| cpal::default_host().default_output_device())
}

// Concrete name a request id resolves to: the guard compares physical
// identity, not the raw (aliasable) request string.
pub(super) fn resolved_device_name(device_id: &str) -> Option<String> {
    output_device_name(&resolve_device(Some(device_id))?)
}

// --- OpenedStream ---

pub(super) struct OpenedStream {
    pub stream: cpal::Stream,
    pub producer: rtrb::Producer<f32>,
    pub rate: u32,
    pub channels: u16,
    pub seek_gen: Arc<AtomicU32>,
    pub muted: Arc<AtomicBool>,
    pub mute_ack: Arc<AtomicBool>,
    pub stream_error: Arc<AtomicU8>,
    pub played_samples: Arc<AtomicU64>,
}

// --- Audio format probing ---

pub(super) struct ProbeInfo {
    pub sample_rate: u32,
    pub channels: u16,
    pub duration: f64,
    pub bit_depth: Option<u32>,
    pub codec: &'static str,
}

pub(super) fn probe_audio_format(
    buffer: &crate::player::buffer::RamBuffer,
) -> Result<ProbeInfo, String> {
    use symphonia::core::codecs::CodecParameters;
    use symphonia::core::formats::probe::Hint;
    use symphonia::core::io::MediaSourceStream;

    let reader = buffer.clone();
    let mss = MediaSourceStream::new(Box::new(reader), Default::default());
    let format = symphonia::default::get_probe()
        .probe(
            &Hint::new(),
            mss,
            symphonia::core::formats::FormatOptions::default(),
            symphonia::core::meta::MetadataOptions::default(),
        )
        .map_err(|e| format!("Probe failed: {e}"))?;

    let (track, params) = format
        .tracks()
        .iter()
        .find_map(|t| match &t.codec_params {
            Some(CodecParameters::Audio(p)) => Some((t, p)),
            _ => None,
        })
        .ok_or_else(|| "No audio track found".to_string())?;

    let sample_rate = params.sample_rate.unwrap_or(44100);
    Ok(ProbeInfo {
        sample_rate,
        channels: params
            .channels
            .as_ref()
            .map(|c| c.count() as u16)
            .unwrap_or(2),
        duration: track
            .num_frames
            .map(|n| n as f64 / sample_rate as f64)
            .unwrap_or(0.0),
        bit_depth: params.bits_per_sample,
        codec: codec_name(params.codec),
    })
}

// --- cpal callback ---

fn build_cpal_callback(
    mut consumer: rtrb::Consumer<f32>,
    volume: Arc<AtomicU32>,
    seek_gen: Arc<AtomicU32>,
    muted: Arc<AtomicBool>,
    mute_ack: Arc<AtomicBool>,
    played_samples: Arc<AtomicU64>,
) -> impl FnMut(&mut [f32], &cpal::OutputCallbackInfo) + Send + 'static {
    let mut local_gen: u32 = 0;
    move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
        let c = &mut consumer;

        if muted.load(Relaxed) {
            let n = c.slots();
            if n > 0
                && let Ok(chunk) = c.read_chunk(n)
            {
                chunk.commit_all();
            }
            for s in data.iter_mut() {
                *s = 0.0;
            }
            mute_ack.store(true, Relaxed);
            return;
        }

        // Seek gen changed - drain stale samples from before the seek
        let cur_gen = seek_gen.load(Relaxed);
        if cur_gen != local_gen {
            let n = c.slots();
            if n > 0
                && let Ok(chunk) = c.read_chunk(n)
            {
                chunk.commit_all();
            }
            local_gen = cur_gen;
        }

        let v = f32::from_bits(volume.load(Relaxed));
        let to_read = c.slots().min(data.len());
        if to_read > 0
            && let Ok(chunk) = c.read_chunk(to_read)
        {
            let (s1, s2) = chunk.as_slices();
            let split = s1.len();
            for (dst, src) in data[..split].iter_mut().zip(s1.iter()) {
                *dst = *src * v;
            }
            for (dst, src) in data[split..to_read].iter_mut().zip(s2.iter()) {
                *dst = *src * v;
            }
            chunk.commit_all();
            played_samples.fetch_add(to_read as u64, Relaxed);
        }
        for s in data[to_read..].iter_mut() {
            *s = 0.0;
        }
    }
}

/// Choose a CPAL output buffer size.
///
/// On Linux (ALSA/PipeWire) the `Default` period can land around 10-20 ms,
/// which underruns easily under VM scheduling jitter. We query the device's
/// supported range and pick the power of two nearest to 100 ms, clamped into
/// that range. On other platforms, or when the device does not advertise a
/// range, we return `Default` unchanged.
fn preferred_buffer_size(_device: &cpal::Device, _rate: u32) -> cpal::BufferSize {
    #[cfg(target_os = "linux")]
    {
        let target = ((_rate as usize * 100) / 1000).next_power_of_two() as u32;
        if let Ok(configs) = _device.supported_output_configs() {
            for cfg in configs {
                if cfg.min_sample_rate() <= _rate
                    && _rate <= cfg.max_sample_rate()
                    && let cpal::SupportedBufferSize::Range { min, max } = cfg.buffer_size()
                {
                    return cpal::BufferSize::Fixed(target.clamp(*min, *max));
                }
            }
        }
    }
    cpal::BufferSize::Default
}

// --- open_output_stream ---

/// Build a cpal output stream for one `StreamConfig` and wire it to a fresh ring
/// buffer. The 5 control atomics are taken by `&Arc` and cloned in (a cpal
/// callback owns what it captures, and each call needs its own consumer/callback);
/// `rate`/`channels` are read back off `config`. Returns the build error; the caller may
/// ignore it (fall through to a fallback config) or log it.
fn open_with_config(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    volume: &Arc<AtomicU32>,
    seek_gen: &Arc<AtomicU32>,
    muted: &Arc<AtomicBool>,
    mute_ack: &Arc<AtomicBool>,
    stream_error: &Arc<AtomicU8>,
    played_samples: &Arc<AtomicU64>,
) -> Result<OpenedStream, cpal::Error> {
    let ring_size = config.sample_rate as usize * config.channels as usize * 2;
    let (producer, consumer) = rtrb::RingBuffer::new(ring_size);

    let cb = build_cpal_callback(
        consumer,
        volume.clone(),
        seek_gen.clone(),
        muted.clone(),
        mute_ack.clone(),
        played_samples.clone(),
    );
    let err_flag = stream_error.clone();
    let stream = device.build_output_stream(
        *config,
        cb,
        move |err: cpal::Error| match err.kind() {
            // Default device changed: cpal auto-rerouted the live stream. Informational only;
            // must NOT write the flag, or it could clear a device-loss signal set moments earlier.
            cpal::ErrorKind::DeviceChanged => {
                crate::vprintln!("[CPAL]   Auto-rerouted to new default device");
            }
            // Device lost / stream invalidated: rebuild on the current default device.
            cpal::ErrorKind::DeviceNotAvailable | cpal::ErrorKind::StreamInvalidated => {
                crate::vprintln!("[CPAL]   Stream error (device lost): {err}");
                err_flag.store(STREAM_ERR_DEVICE_LOST, Relaxed);
            }
            _ => {
                crate::vprintln!("[CPAL]   Stream error: {err}");
                err_flag.store(STREAM_ERR_UNKNOWN, Relaxed);
            }
        },
        None,
    )?;
    Ok(OpenedStream {
        stream,
        producer,
        rate: config.sample_rate,
        channels: config.channels,
        seek_gen: seek_gen.clone(),
        muted: muted.clone(),
        mute_ack: mute_ack.clone(),
        stream_error: stream_error.clone(),
        played_samples: played_samples.clone(),
    })
}

pub(super) fn open_output_stream(
    device: &cpal::Device,
    source_rate: u32,
    source_channels: u16,
    volume: &Arc<AtomicU32>,
) -> Option<OpenedStream> {
    let seek_gen = Arc::new(AtomicU32::new(0));
    let muted = Arc::new(AtomicBool::new(false));
    let mute_ack = Arc::new(AtomicBool::new(false));
    let stream_error = Arc::new(AtomicU8::new(0));
    let played_samples = Arc::new(AtomicU64::new(0));

    let dev_name = output_device_name(device).unwrap_or_else(|| "<unknown>".to_string());
    crate::vprintln!("[CPAL]   Device: {}", dev_name);

    // Attempt 1: source rate (no software resampling needed)
    let config = cpal::StreamConfig {
        channels: source_channels,
        sample_rate: source_rate,
        buffer_size: preferred_buffer_size(device, source_rate),
    };
    if let Ok(opened) = open_with_config(
        device,
        &config,
        volume,
        &seek_gen,
        &muted,
        &mute_ack,
        &stream_error,
        &played_samples,
    ) {
        crate::vprintln!(
            "[CPAL]   Opened at source rate: {}Hz/{}ch",
            source_rate,
            source_channels
        );
        return Some(opened);
    }

    // Attempt 2: device default (will need rubato resampling)
    let default = device.default_output_config().ok()?;
    let ar = default.sample_rate();
    let ac = default.channels();
    let cfg = cpal::StreamConfig {
        channels: ac,
        sample_rate: ar,
        buffer_size: preferred_buffer_size(device, ar),
    };

    crate::vprintln!(
        "[CPAL]   Source rate {}Hz unsupported, using device default: {}Hz/{}ch",
        source_rate,
        ar,
        ac
    );

    match open_with_config(
        device,
        &cfg,
        volume,
        &seek_gen,
        &muted,
        &mute_ack,
        &stream_error,
        &played_samples,
    ) {
        Ok(opened) => Some(opened),
        Err(e) => {
            crate::vprintln!("[ERROR]  Failed to open cpal stream: {e}");
            None
        }
    }
}

// --- AudioPipeline ---

/// Source frames the resampler takes per call. Shared with the tests, which size their
/// fixtures against it: a private literal would let the two drift apart in silence.
pub(super) const CHUNK_SIZE: usize = 1024;

pub(super) struct AudioPipeline {
    /// Absent when source and output share a rate: a sinc filter at ratio 1.0 returns
    /// samples that were already correct, minus a low-pass and a lead-in delay. Rate parity
    /// gets the channel remap alone.
    resampler: Option<rubato::Async<f32>>,
    source_channels: usize,
    output_channels: usize,
    accum: Vec<Vec<f32>>,
    accum_frames: usize,
}

impl AudioPipeline {
    /// Fails rather than panics: the rate pair comes from a container header and a device,
    /// neither of which this thread controls, and a panic here takes the decode thread down
    /// without a `Finished` or an `Error` for the player to act on.
    pub fn new(
        source_rate: u32,
        output_rate: u32,
        source_channels: usize,
        output_channels: usize,
    ) -> Result<Self, String> {
        let resampler = if source_rate == output_rate {
            None
        } else {
            let params = rubato::SincInterpolationParameters {
                sinc_len: 256,
                f_cutoff: Some(0.95),
                interpolation: rubato::SincInterpolationType::Linear,
                oversampling_factor: 256,
                window: rubato::WindowFunction::BlackmanHarris2,
            };
            Some(
                rubato::Async::<f32>::new_sinc(
                    output_rate as f64 / source_rate as f64,
                    2.0,
                    &params,
                    CHUNK_SIZE,
                    source_channels,
                    rubato::FixedAsync::Input,
                )
                .map_err(|e| format!("resampler {source_rate} -> {output_rate}: {e}"))?,
            )
        };

        // The accumulator stages frames for the resampler and has no second reader: rate parity
        // leaves it unbuilt rather than holding a buffer `process` and `flush` both skip.
        let accum = if resampler.is_some() {
            vec![Vec::with_capacity(CHUNK_SIZE * 2); source_channels]
        } else {
            Vec::new()
        };

        Ok(Self {
            resampler,
            source_channels,
            output_channels,
            accum,
            accum_frames: 0,
        })
    }

    pub fn resamples(&self) -> bool {
        self.resampler.is_some()
    }

    /// Errors instead of dropping the chunk: a gated log paired with an unconditional drain
    /// lets a whole track vanish under normal-looking playback.
    pub fn process(&mut self, interleaved: &[f32]) -> Result<Vec<f32>, String> {
        use audioadapter_buffers::direct::SequentialSliceOfVecs;

        let Self {
            resampler,
            source_channels,
            output_channels,
            accum,
            accum_frames,
        } = self;
        let (src_ch, out_ch) = (*source_channels, *output_channels);

        let Some(resampler) = resampler.as_mut() else {
            let mut output = Vec::new();
            remap_channels(interleaved, src_ch, out_ch, &mut output);
            return Ok(output);
        };

        let frames = interleaved.len() / src_ch;
        for f in 0..frames {
            for ch in 0..src_ch {
                accum[ch].push(interleaved[f * src_ch + ch]);
            }
        }
        *accum_frames += frames;

        let mut output = Vec::new();
        while *accum_frames >= CHUNK_SIZE {
            let data = {
                let adapter = SequentialSliceOfVecs::new(accum, src_ch, CHUNK_SIZE)
                    .map_err(|e| format!("resampler input rejected: {e}"))?;
                resampler
                    .process(&adapter, None)
                    .map_err(|e| format!("resampling failed: {e}"))?
                    .take_data()
            };
            remap_channels(&data, src_ch, out_ch, &mut output);

            for ch in accum.iter_mut() {
                ch.drain(..CHUNK_SIZE);
            }
            *accum_frames -= CHUNK_SIZE;
        }

        Ok(output)
    }

    pub fn flush(&mut self) -> Result<Vec<f32>, String> {
        use audioadapter_buffers::direct::SequentialSliceOfVecs;
        use rubato::Indexing;

        let Self {
            resampler,
            source_channels,
            output_channels,
            accum,
            accum_frames,
        } = self;
        let (src_ch, out_ch) = (*source_channels, *output_channels);

        // Nothing accumulates without a resampler: `process` remaps and returns in one go.
        let Some(resampler) = resampler.as_mut() else {
            return Ok(Vec::new());
        };
        if *accum_frames == 0 {
            return Ok(Vec::new());
        }

        let partial_frames = *accum_frames;
        for ch in accum.iter_mut() {
            ch.resize(CHUNK_SIZE, 0.0);
        }

        let out_max = resampler.output_frames_max();
        let mut out_buf =
            audioadapter_buffers::owned::InterleavedOwned::<f32>::new(0.0, src_ch, out_max);

        let indexing = Indexing {
            input_offset: 0,
            output_offset: 0,
            partial_len: Some(partial_frames),
            active_channels_mask: None,
        };

        let written = {
            let adapter = SequentialSliceOfVecs::new(accum, src_ch, CHUNK_SIZE)
                .map_err(|e| format!("resampler input rejected: {e}"))?;
            let (_consumed, written) = resampler
                .process_into_buffer(&adapter, &mut out_buf, Some(&indexing))
                .map_err(|e| format!("flushing the resampler failed: {e}"))?;
            written
        };

        let mut output = Vec::new();
        let data = out_buf.take_data();
        remap_channels(&data[..written * src_ch], src_ch, out_ch, &mut output);

        for ch in accum.iter_mut() {
            ch.clear();
        }
        *accum_frames = 0;
        Ok(output)
    }

    pub fn reset(&mut self) {
        for ch in &mut self.accum {
            ch.clear();
        }
        self.accum_frames = 0;
        if let Some(resampler) = self.resampler.as_mut() {
            // The sinc history holds up to `sinc_len` frames of pre-seek audio; leaving it
            // loaded bleeds them into the first output after the seek.
            resampler.reset();
        }
    }
}

/// Free function rather than a method: `process` holds a split borrow of the accumulator
/// and the resampler, which a `&self` receiver would conflict with.
fn remap_channels(data: &[f32], src_ch: usize, out_ch: usize, output: &mut Vec<f32>) {
    let frames = data.len() / src_ch;
    output.reserve(frames * out_ch);

    if src_ch == out_ch {
        output.extend_from_slice(data);
        return;
    }

    for f_idx in 0..frames {
        for ch in 0..out_ch {
            let sample = if ch < src_ch {
                data[f_idx * src_ch + ch]
            } else if src_ch == 1 {
                data[f_idx * src_ch] // mono -> multi: duplicate
            } else {
                0.0 // extra channels: silence
            };
            output.push(sample);
        }
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/player/thread/output.rs"]
mod tests;
