use cpal::traits::{DeviceTrait, HostTrait};
use rubato::Resampler;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering::Relaxed};

use crate::player::AudioDevice;

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

pub(super) fn codec_name(codec: symphonia::core::codecs::CodecType) -> &'static str {
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

pub(super) fn find_output_device(device_id: &str) -> Option<cpal::Device> {
    let host = cpal::default_host();
    if device_id == "default" {
        return host.default_output_device();
    }

    host.output_devices().ok()?.find(|d| {
        d.description()
            .ok()
            .map(|desc| desc.name() == device_id)
            .unwrap_or(false)
    })
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
    use symphonia::core::io::MediaSourceStream;

    let reader = buffer.clone();
    let mss = MediaSourceStream::new(Box::new(reader), Default::default());
    let probed = symphonia::default::get_probe()
        .format(
            &symphonia::core::probe::Hint::new(),
            mss,
            &symphonia::core::formats::FormatOptions::default(),
            &symphonia::core::meta::MetadataOptions::default(),
        )
        .map_err(|e| format!("Probe failed: {e}"))?;

    let track = probed
        .format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != symphonia::core::codecs::CODEC_TYPE_NULL)
        .ok_or_else(|| "No audio track found".to_string())?;

    let sample_rate = track.codec_params.sample_rate.unwrap_or(44100);
    Ok(ProbeInfo {
        sample_rate,
        channels: track
            .codec_params
            .channels
            .map(|c| c.count() as u16)
            .unwrap_or(2),
        duration: track
            .codec_params
            .n_frames
            .map(|n| n as f64 / sample_rate as f64)
            .unwrap_or(0.0),
        bit_depth: track.codec_params.bits_per_sample,
        codec: codec_name(track.codec_params.codec),
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
/// supported range and pick the power of two nearest to ~100 ms clamped into
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

    let dev_name = device
        .description()
        .map(|d| d.name().to_string())
        .unwrap_or_else(|_| "<unknown>".to_string());
    crate::vprintln!("[CPAL]   Device: {}", dev_name);

    // Attempt 1: source rate (no software resampling needed)
    {
        let config = cpal::StreamConfig {
            channels: source_channels,
            sample_rate: source_rate,
            buffer_size: preferred_buffer_size(device, source_rate),
        };
        let ring_size = source_rate as usize * source_channels as usize * 2;
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
                mute_ack,
                stream_error,
                played_samples,
            });
        }
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

    let ring_size = ar as usize * ac as usize * 2;
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
            mute_ack,
            stream_error,
            played_samples,
        }),
        Err(e) => {
            eprintln!("[ERROR]  Failed to open cpal stream: {e}");
            None
        }
    }
}

// --- AudioPipeline ---

pub(super) struct AudioPipeline {
    resampler: rubato::Async<f32>,
    source_channels: usize,
    output_channels: usize,
    chunk_size: usize,
    accum: Vec<Vec<f32>>,
    accum_frames: usize,
}

impl AudioPipeline {
    pub fn new(
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

        let resampler = rubato::Async::<f32>::new_sinc(
            ratio,
            2.0,
            &params,
            chunk_size,
            source_channels,
            rubato::FixedAsync::Input,
        )
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

    pub fn process(&mut self, interleaved: &[f32]) -> Vec<f32> {
        use audioadapter_buffers::direct::SequentialSliceOfVecs;
        use rubato::Resampler;

        let src_ch = self.source_channels;
        let frames = interleaved.len() / src_ch;

        for f in 0..frames {
            for ch in 0..src_ch {
                self.accum[ch].push(interleaved[f * src_ch + ch]);
            }
        }
        self.accum_frames += frames;

        let mut output = Vec::new();

        while self.accum_frames >= self.chunk_size {
            let adapter = SequentialSliceOfVecs::new(&self.accum, src_ch, self.chunk_size)
                .expect("invalid buffer dimensions");

            match self.resampler.process(&adapter, 0, None) {
                Ok(resampled) => {
                    let data = resampled.take_data();
                    self.remap_channels(&data, src_ch, &mut output);
                }
                Err(e) => {
                    crate::vprintln!("[RESAMPLE] Error: {e}");
                }
            }

            for ch in &mut self.accum {
                ch.drain(..self.chunk_size);
            }
            self.accum_frames -= self.chunk_size;
        }

        output
    }

    pub fn flush(&mut self) -> Vec<f32> {
        use audioadapter_buffers::direct::SequentialSliceOfVecs;
        use rubato::{Indexing, Resampler};

        if self.accum_frames == 0 {
            return Vec::new();
        }

        let src_ch = self.source_channels;
        let partial_frames = self.accum_frames;

        for ch in &mut self.accum {
            ch.resize(self.chunk_size, 0.0);
        }

        let adapter = SequentialSliceOfVecs::new(&self.accum, src_ch, self.chunk_size)
            .expect("invalid buffer dimensions");

        let out_max = self.resampler.output_frames_max();
        let mut out_buf =
            audioadapter_buffers::owned::InterleavedOwned::<f32>::new(0.0, src_ch, out_max);

        let indexing = Indexing {
            input_offset: 0,
            output_offset: 0,
            partial_len: Some(partial_frames),
            active_channels_mask: None,
        };

        let mut output = Vec::new();
        match self
            .resampler
            .process_into_buffer(&adapter, &mut out_buf, Some(&indexing))
        {
            Ok((_consumed, written)) => {
                let data = out_buf.take_data();
                let used = &data[..written * src_ch];
                self.remap_channels(used, src_ch, &mut output);
            }
            Err(e) => {
                crate::vprintln!("[RESAMPLE] Flush error: {e}");
            }
        }

        for ch in &mut self.accum {
            ch.clear();
        }
        self.accum_frames = 0;
        output
    }

    pub fn reset(&mut self) {
        for ch in &mut self.accum {
            ch.clear();
        }
        self.accum_frames = 0;
        self.resampler.reset();
    }

    fn remap_channels(&self, data: &[f32], src_ch: usize, output: &mut Vec<f32>) {
        let out_ch = self.output_channels;
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
                    data[f_idx * src_ch] // mono → multi: duplicate
                } else {
                    0.0 // extra channels: silence
                };
                output.push(sample);
            }
        }
    }
}
