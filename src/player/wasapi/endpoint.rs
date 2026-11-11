use std::sync::mpsc;

use cpal::traits::DeviceTrait;

use wasapi::{
    AudioClient, AudioRenderClient, DeviceEnumerator, Direction, Handle, SampleType, StreamMode,
    WaveFormat, calculate_period_100ns,
};

use super::ExclusiveEvent;
use super::render::timed_endpoint;
use crate::player::thread::output::{find_output_device, is_default_selector};

fn negotiate_format(
    sample_rate: u32,
    channels: u32,
    source_bps: u32,
) -> Vec<(WaveFormat, i64, String)> {
    let sr = sample_rate as usize;
    let ch = channels as usize;
    let bps = source_bps as usize;

    let channel_mask = wasapi::make_channelmasks(ch).first().copied();

    // Each candidate carries a "container/valid type" label for the negotiation
    // logs to identify exactly which shape a driver accepted or rejected.
    let mut candidates = Vec::new();

    // Priority 1: 32-bit container with source valid bits (integer)
    candidates.push((
        WaveFormat::new(32, bps.min(32), SampleType::Int, sr, ch, channel_mask),
        format!("32c/{}v Int", bps.min(32)),
    ));
    // Priority 2: 24-bit container with 24 valid bits
    if bps != 24 {
        candidates.push((
            WaveFormat::new(24, 24, SampleType::Int, sr, ch, channel_mask),
            "24c/24v Int".to_string(),
        ));
    }
    // Priority 3: 16-bit container with 16 valid bits
    if bps != 16 {
        candidates.push((
            WaveFormat::new(16, 16, SampleType::Int, sr, ch, channel_mask),
            "16c/16v Int".to_string(),
        ));
    }
    // Priority 4: 32-bit float
    candidates.push((
        WaveFormat::new(32, 32, SampleType::Float, sr, ch, channel_mask),
        "32c/32v Float".to_string(),
    ));

    // 20ms exclusive period: the 10ms minimum leaves no scheduling slack (one
    // late wake underruns), and music tolerates the extra latency. The
    // BUFFER_SIZE_NOT_ALIGNED retry below realigns if the driver needs it.
    const EXCLUSIVE_PERIOD_MS: i64 = 20;
    let period = calculate_period_100ns(
        sr as i64 * EXCLUSIVE_PERIOD_MS / 1000, // frames per period
        sr as i64,
    );

    candidates
        .into_iter()
        .map(|(fmt, label)| (fmt, period, label))
        .collect()
}

pub(super) fn init_exclusive_client(
    device_id: &str,
) -> Result<(DeviceEnumerator, wasapi::Device), String> {
    let enumerator = DeviceEnumerator::new().map_err(|e| format!("DeviceEnumerator: {e}"))?;

    // "auto"/"default"/empty are the default-device sentinels: resolve them to the OS default
    // render endpoint instead of a literal device-name lookup.
    let device = if is_default_selector(device_id) || device_id.is_empty() {
        enumerator
            .get_default_device(Direction::Render)
            .map_err(|e| format!("default device: {e}"))?
    } else {
        // `device_id` is cpal's device name: FriendlyName, or DeviceDesc when a driver
        // sets none. Letting cpal resolve it and opening its endpoint id picks the device
        // the shared path would, which matching either property does not guarantee.
        let endpoint = find_output_device(device_id)
            .and_then(|d| d.id().ok())
            .ok_or_else(|| format!("device '{device_id}': not found"))?;
        enumerator
            .get_device(&endpoint.1)
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
    crate::vprintln!(
        "[WASAPI] Exclusive negotiate: {}Hz {}ch source {}bit, {} candidates",
        sample_rate,
        channels,
        source_bps,
        candidates.len()
    );

    for (idx, (wave_fmt, period, label)) in candidates.iter().enumerate() {
        // One line per attempt rather than the list up front: the timestamps show how long
        // each candidate held the driver, which a single list cannot.
        crate::vprintln!(
            "[WASAPI] Candidate {}/{}: {label}",
            idx + 1,
            candidates.len()
        );
        let mut audio_client = device
            .get_iaudioclient()
            .map_err(|e| format!("get_iaudioclient: {e}"))?;

        let stream_mode = StreamMode::EventsExclusive {
            period_hns: *period,
        };

        // Bound the borrow to this statement: the arms below use `audio_client` again.
        let initialized = timed_endpoint(format_args!("initialize ({label})"), || {
            audio_client.initialize_client(wave_fmt, Direction::Render, stream_mode, None)
        });
        match initialized {
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
                    "[WASAPI] Exclusive stream opened ({label}): {}Hz {}ch {}bit, buffer={}frames",
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

                        let realigned =
                            timed_endpoint(format_args!("initialize (aligned, {label})"), || {
                                audio_client2.initialize_client(
                                    wave_fmt,
                                    Direction::Render,
                                    stream_mode2,
                                    None,
                                )
                            });
                        if realigned.is_ok() {
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
                                "[WASAPI] Exclusive stream opened (aligned, {label}): {}Hz {}ch {}bit, buffer={}frames",
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
                    // Names the candidate: the abort hides which one met the lock, and
                    // whether any remained untried behind it.
                    crate::vprintln!(
                        "[WASAPI] Device in use on {label}, {} candidate(s) left untried: {e}",
                        candidates.len() - idx - 1
                    );
                    return Err(err_str);
                }
                // Exclusive disabled for the device in Windows: every candidate
                // returns the same error; stop probing and propagate it (lands
                // in InitFailed -> ExclusiveModeNotAllowed, the permanent class).
                if is_exclusive_mode_disabled_error(&err_str) {
                    crate::vprintln!(
                        "[WASAPI] Exclusive mode disabled for this device in Windows \
                         (Sound > device > Advanced > Allow exclusive control), on {label}: {e}"
                    );
                    return Err(err_str);
                }
                // "Refused", not "format rejected": this arm also catches
                // ENDPOINT_CREATE_FAILED, which reports a device the driver could not
                // configure rather than a format it does not support.
                crate::vprintln!("[WASAPI] Candidate refused ({label}): {e}");
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
/// 0x8889000E), not a format-level rejection. No other candidate can succeed.
fn is_exclusive_mode_disabled_error(err_str: &str) -> bool {
    err_str.contains("EXCLUSIVE_MODE_NOT_ALLOWED")
        || err_str.contains("8889000e")
        || err_str.contains("8889000E")
}

/// Convert source PCM bytes to the format expected by the WASAPI device.
/// Source is always integer PCM at `src_bps` bits, output is at `dst_store_bits`/`dst_valid_bits`.
pub(super) fn convert_pcm_frame(
    src: &[u8],
    src_bps: u32,
    dst_store_bits: u32,
    _dst_valid_bits: u32,
    dst_sample_type: &SampleType,
    _channels: u32,
    gain: f32,
    out: &mut Vec<u8>,
) {
    let src_bytes_per_sample = (src_bps / 8) as usize;
    let dst_bytes_per_sample = (dst_store_bits / 8) as usize;
    let num_samples = src.len() / src_bytes_per_sample;
    out.clear();
    out.reserve(num_samples * dst_bytes_per_sample);

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
}

/// Open a new exclusive stream and dispatch error events on failure. Returns Ok with the
/// new resources, or Err(()) when an error event has been sent and the render thread
/// should exit. The caller has already dropped whatever client it held.
pub(super) fn try_open_stream(
    device: &wasapi::Device,
    stream_id: u32,
    sample_rate: u32,
    channels: u32,
    bits_per_sample: u32,
    event_tx: &mpsc::Sender<ExclusiveEvent>,
) -> Result<(AudioClient, AudioRenderClient, Handle, WaveFormat, u32), ()> {
    match open_exclusive_stream(device, sample_rate, channels, bits_per_sample) {
        Ok(resources) => Ok(resources),
        Err(e) => {
            crate::vprintln!("[WASAPI] Failed to open stream: {e}");
            if is_device_in_use_error(&e) {
                let _ = event_tx.send(ExclusiveEvent::DeviceLocked(e));
            } else if e.as_str() == "no compatible exclusive format found" {
                // Per-track: this track's format isn't exclusive-compatible, but exclusive
                // itself is fine for other rates. Play shared, keep exclusive on (do NOT treat
                // it as a device-wide ExclusiveModeNotAllowed, which would disable exclusive).
                let _ = event_tx.send(ExclusiveEvent::FormatUnsupported { stream_id });
            } else {
                let _ = event_tx.send(ExclusiveEvent::InitFailed(e));
            }
            Err(())
        }
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/player/wasapi/endpoint.rs"]
mod tests;
