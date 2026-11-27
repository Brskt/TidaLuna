use cpal::traits::{DeviceTrait, HostTrait};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering::Relaxed};

use super::mixer::{CrossfadeLink, OutputGain, build_cpal_callback};
use super::{STREAM_ERR_DEVICE_LOST, STREAM_ERR_UNKNOWN};
use crate::player::{AudioDevice, DeviceErrorKind};

/// Why an output would not open, as cpal's build error tells it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::player::thread) enum OpenFailure {
    Format,
    Gone,
    Busy,
    Other,
}

impl OpenFailure {
    /// What the listener hears: only a refused format says "format".
    pub(in crate::player::thread) fn device_error(self) -> DeviceErrorKind {
        match self {
            Self::Format => DeviceErrorKind::FormatNotSupported,
            Self::Gone => DeviceErrorKind::NotFound,
            Self::Busy => DeviceErrorKind::Locked,
            Self::Other => DeviceErrorKind::Unknown,
        }
    }
}

/// Read a failed open from cpal's error kind.
pub(in crate::player::thread) fn classify_open_error(kind: cpal::ErrorKind) -> OpenFailure {
    match kind {
        cpal::ErrorKind::UnsupportedConfig => OpenFailure::Format,
        cpal::ErrorKind::DeviceNotAvailable => OpenFailure::Gone,
        cpal::ErrorKind::DeviceBusy => OpenFailure::Busy,
        _ => OpenFailure::Other,
    }
}

/// cpal's WASAPI panics on endpoint calls Microsoft documents as fallible (`Item`, `GetDataFlow`,
/// `OpenPropertyStore`), and hotplug reaches them while endpoints come and go. Read as the error
/// cpal would return; drop once the pinned cpal rev returns one there.
fn without_cpal_panic<T>(failed: T, read: impl FnOnce() -> T) -> T {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(read)).unwrap_or(failed)
}

pub(in crate::player::thread) fn enumerate_audio_devices() -> Vec<AudioDevice> {
    let host = cpal::default_host();
    let mut devices = vec![AudioDevice {
        controllable_volume: true,
        id: "default".to_string(),
        name: "System Default".to_string(),
        r#type: Some("systemDefault".to_string()),
    }];

    let names = without_cpal_panic(None, || {
        let outputs = host.output_devices().ok()?;
        Some(
            outputs
                .filter_map(|device| device.description().ok())
                .map(|desc| desc.name().to_string())
                .collect::<Vec<_>>(),
        )
    });
    for name in names.into_iter().flatten() {
        devices.push(AudioDevice {
            controllable_volume: true,
            id: name.clone(),
            name,
            r#type: None,
        });
    }

    devices
}

/// True when `id` selects the OS default device rather than a specific one:
/// `"default"` (the device-list entry) and `"auto"` (selectSystemDevice) are aliases.
pub(crate) fn is_default_selector(id: &str) -> bool {
    id == "default" || id == "auto"
}

pub(in crate::player) fn find_output_device(device_id: &str) -> Option<cpal::Device> {
    let host = cpal::default_host();
    // A default selector maps to the OS default without enumerating.
    if is_default_selector(device_id) {
        return host.default_output_device();
    }

    without_cpal_panic(None, || {
        host.output_devices().ok()?.find(|d| {
            d.description()
                .ok()
                .map(|desc| desc.name() == device_id)
                .unwrap_or(false)
        })
    })
}

pub(in crate::player::thread) fn output_device_name(device: &cpal::Device) -> Option<String> {
    without_cpal_panic(None, || {
        device.description().ok().map(|d| d.name().to_string())
    })
}

pub(in crate::player::thread) fn resolve_device(device_id: Option<&str>) -> Option<cpal::Device> {
    pick_device(device_id, find_output_device, || {
        cpal::default_host().default_output_device()
    })
}

/// The device a request opens: the OS default when nothing was asked, never in place of a named
/// device that is gone (the official engine answers `notFound` there, with no fallback).
pub(in crate::player::thread) fn pick_device<D>(
    device_id: Option<&str>,
    find: impl FnOnce(&str) -> Option<D>,
    default: impl FnOnce() -> Option<D>,
) -> Option<D> {
    match device_id {
        Some(id) => find(id),
        None => default(),
    }
}

// Concrete name a request id resolves to: the guard compares physical
// identity, not the raw (aliasable) request string.
pub(in crate::player::thread) fn resolved_device_name(device_id: &str) -> Option<String> {
    output_device_name(&resolve_device(Some(device_id))?)
}

pub(in crate::player::thread) struct OpenedStream {
    pub stream: cpal::Stream,
    pub producer: rtrb::Producer<f32>,
    pub rate: u32,
    pub channels: u16,
    pub seek_gen: Arc<AtomicU32>,
    pub muted: Arc<AtomicBool>,
    pub mute_ack: Arc<AtomicBool>,
    pub stream_error: Arc<AtomicU8>,
    pub played_samples: Arc<AtomicU64>,
    /// Shared with the callback for the whole life of the stream. `attach` offers a slot
    /// (`Some` = new, NEVER "cancel": one cell for both made every fade release itself the tick
    /// after adoption), `cancel` is the one-shot that drops an adopted one, and `done` packs
    /// generation and origin sample in one word, so a fresh generation can never be read beside
    /// a stale origin.
    pub xfade: CrossfadeLink,
}

/// Choose a CPAL output buffer size.
///
/// On Linux (ALSA/PipeWire) the `Default` period can land at 10-20 ms,
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

/// Build a cpal output stream for one `StreamConfig` and wire it to a fresh ring
/// buffer. The 5 control atomics are taken by `&Arc` and cloned in (a cpal
/// callback owns what it captures, and each call needs its own consumer/callback);
/// `rate`/`channels` are read back off `config`. Returns the build error; the caller may
/// ignore it (fall through to a fallback config) or log it.
fn open_with_config(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    gain: &OutputGain,
    seek_gen: &Arc<AtomicU32>,
    muted: &Arc<AtomicBool>,
    mute_ack: &Arc<AtomicBool>,
    stream_error: &Arc<AtomicU8>,
    played_samples: &Arc<AtomicU64>,
) -> Result<OpenedStream, cpal::Error> {
    let ring_size = config.sample_rate as usize * config.channels as usize * 2;
    let (producer, consumer) = rtrb::RingBuffer::new(ring_size);

    let xfade = CrossfadeLink::new();

    let cb = build_cpal_callback(
        consumer,
        gain.clone(),
        seek_gen.clone(),
        muted.clone(),
        mute_ack.clone(),
        played_samples.clone(),
        xfade.clone(),
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
        xfade,
    })
}

/// Whether the output stream is pinned to the device's own rate for the life of the
/// device, instead of being reopened at each track's rate.
///
/// True where the device's reported default IS the rate the audio server runs: WASAPI shared
/// mode returns the endpoint's mix format, CoreAudio the device's live stream format. False on
/// Linux, where cpal reaches the device through pipewire-alsa's `default` PCM, whose hardcoded
/// [1, 384000] range is unrelated to the graph's clock and resolves to 48000: aiming at that
/// would stack our conversion on PipeWire's instead of replacing it.
pub(crate) const ENGINE_RATE_IS_PINNED: bool =
    cfg!(any(target_os = "windows", target_os = "macos"));

/// The two configurations to try, in order. The second is the fallback: the caller
/// still opens when a device refuses its own advertised default. `pinned` is read off
/// `ENGINE_RATE_IS_PINNED` by the caller rather than here, keeping both orderings
/// testable on any host.
fn attempt_order(source: (u32, u16), device_default: (u32, u16), pinned: bool) -> [(u32, u16); 2] {
    if pinned {
        [device_default, source]
    } else {
        [source, device_default]
    }
}

/// A named device gone between its lookup and this open reaches the same cpal panics.
pub(in crate::player::thread) fn open_output_stream(
    device: &cpal::Device,
    source_rate: u32,
    source_channels: u16,
    gain: &OutputGain,
) -> Result<OpenedStream, OpenFailure> {
    without_cpal_panic(Err(OpenFailure::Other), || {
        open_on_device(device, source_rate, source_channels, gain)
    })
}

fn open_on_device(
    device: &cpal::Device,
    source_rate: u32,
    source_channels: u16,
    gain: &OutputGain,
) -> Result<OpenedStream, OpenFailure> {
    let seek_gen = Arc::new(AtomicU32::new(0));
    let muted = Arc::new(AtomicBool::new(false));
    let mute_ack = Arc::new(AtomicBool::new(false));
    let stream_error = Arc::new(AtomicU8::new(0));
    let played_samples = Arc::new(AtomicU64::new(0));

    let dev_name = output_device_name(device).unwrap_or_else(|| "<unknown>".to_string());
    crate::vprintln!("[CPAL]   Device: {}", dev_name);

    // Read late, never cached: on macOS the device's nominal rate is a shared, mutable
    // setting another application can change between the query and the open.
    let default = device.default_output_config().ok();
    let source = (source_rate, source_channels);
    let candidates = match default {
        Some(ref d) => attempt_order(
            source,
            (d.sample_rate(), d.channels()),
            ENGINE_RATE_IS_PINNED,
        ),
        None => [source, source],
    };

    let mut last_err = None;
    for (i, &(rate, channels)) in candidates.iter().enumerate() {
        if i == 1 && candidates[0] == candidates[1] {
            break;
        }
        let config = cpal::StreamConfig {
            channels,
            sample_rate: rate,
            buffer_size: preferred_buffer_size(device, rate),
        };
        match open_with_config(
            device,
            &config,
            gain,
            &seek_gen,
            &muted,
            &mute_ack,
            &stream_error,
            &played_samples,
        ) {
            Ok(opened) => {
                if (rate, channels) == source {
                    crate::vprintln!("[CPAL]   Opened at source rate: {rate}Hz/{channels}ch");
                } else {
                    crate::vprintln!(
                        "[CPAL]   Opened at device rate: {rate}Hz/{channels}ch (source {}Hz, resampling here)",
                        source_rate
                    );
                }
                return Ok(opened);
            }
            Err(e) => {
                crate::vprintln!("[CPAL]   Rejected {rate}Hz/{channels}ch: {e}");
                last_err = Some(e);
            }
        }
    }

    // The last attempt's error names the failure: the loop always makes one.
    Err(last_err.map_or(OpenFailure::Other, |e| {
        crate::vprintln!("[ERROR]  Failed to open cpal stream: {e}");
        classify_open_error(e.kind())
    }))
}

#[cfg(test)]
#[path = "../../../../tests/unit/player/thread/output/stream.rs"]
mod tests;
