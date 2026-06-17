//! ASIO real-time host (Windows only).
//!
//! The driver invokes `bufferSwitch` on its own real-time thread. The four ASIO
//! callbacks are bare C function pointers with no `this`, so they recover state
//! from a process-global pointer; ASIO loads one driver at a time, so a single
//! global fits. The callback must never allocate, lock, block, or panic (a panic
//! crossing `extern "system"` aborts the process).

use core::cell::UnsafeCell;
use core::ffi::c_void;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicPtr, AtomicU32, AtomicU64, Ordering};
use std::collections::VecDeque;
use std::fs::File;
use std::sync::Arc;
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use symphonia::core::codecs::CodecParameters;
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::formats::FormatOptions;
use symphonia::core::formats::probe::Hint;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use windows_sys::Win32::UI::WindowsAndMessaging::GetDesktopWindow;

use super::convert::{AsioSampleType, apply_gain, write_dst_sample};
use super::driver::{AsioDriverInfo, enumerate_asio_drivers};
use super::iasio::{
    AsioBool, AsioBufferInfo, AsioCallbacks, AsioChannelInfo, AsioDriver, AsioSampleRate, AsioTime,
    asio_ok, output_ready_raw,
};
use crate::player::PlaybackState;
use crate::player::buffer::RamBuffer;

/// Stereo: TIDAL streams only stereo PCM, and ASIO4ALL exposes 2 output channels.
const CHANNELS: usize = 2;

/// The state the real-time callback needs, set once before `ASIOStart`. Held
/// behind `TONE_CTX` as a raw pointer because the C callbacks cannot capture.
struct ToneCtx {
    /// Frames per ASIO buffer (the driver's preferred size).
    frames: usize,
    /// The driver's output sample type (queried from `getChannelInfo`).
    dst: AsioSampleType,
    /// Bytes one sample occupies in the ASIO buffer (`dst.bytes_per_sample()`).
    bps: usize,
    /// The driver's current sample rate, in Hz.
    sample_rate: f64,
    /// The two ping/pong output-buffer addresses per channel, filled by
    /// `createBuffers`: `out_buffers[channel][double_buffer_index]`.
    out_buffers: [[*mut u8; 2]; CHANNELS],
    /// Raw `IASIO` COM pointer, so the callback can signal `outputReady`.
    com: *mut core::ffi::c_void,
    /// Absolute frame index of the next buffer, so the tone stays phase-continuous.
    frame_counter: AtomicU64,
    /// Pre-allocated interleaved-i32 scratch (`frames * CHANNELS`). Touched only on
    /// the driver's RT thread between `start` and `stop`, hence the `UnsafeCell`.
    scratch: UnsafeCell<Box<[i32]>>,
}

/// Process-global pointer to the live `ToneCtx`. Set before `ASIOStart`, read by
/// the RT callback, nulled after `ASIOStop`. Null means "no callback work".
static TONE_CTX: AtomicPtr<ToneCtx> = AtomicPtr::new(core::ptr::null_mut());

/// Diagnostic: `bufferSwitch` invocation count (0 = the driver never drove us).
static TONE_SWITCHES: AtomicU64 = AtomicU64::new(0);

/// First-callback diagnostics: null-buffer bitmask and generated sample min/max,
/// to localize silence to our code vs. the driver.
static DBG_NULL_BUFFERS: AtomicU32 = AtomicU32::new(0);
static DBG_SAMPLE_MIN: AtomicI32 = AtomicI32::new(0);
static DBG_SAMPLE_MAX: AtomicI32 = AtomicI32::new(0);

/// Fill `scratch` with one buffer of test tone (interleaved i32, full-scale),
/// phase-continuous across calls via `start_frame`. Smoke-test signal only.
/// RT contract: no allocation, locking, blocking, or panics.
fn fill_tone(scratch: &mut [i32], channels: usize, start_frame: u64, sample_rate: f64) {
    const FREQ: f64 = 440.0;
    const AMP: f64 = 0.2 * i32::MAX as f64; // ~-14 dBFS, headphone-safe
    let frames = scratch.len() / channels;
    for f in 0..frames {
        let n = start_frame + f as u64;
        let phase = core::f64::consts::TAU * (n as f64) * FREQ / sample_rate;
        let sample = (AMP * phase.sin()) as i32;
        for ch in 0..channels {
            scratch[f * channels + ch] = sample;
        }
    }
}

/// `bufferSwitch`: the driver asks us to fill output half `double_buffer_index`
/// (the other half is playing). Runs on the driver's real-time thread.
unsafe extern "system" fn tone_buffer_switch(double_buffer_index: i32, _direct_process: AsioBool) {
    let first = TONE_SWITCHES.fetch_add(1, Ordering::Relaxed) == 0;
    let ctx = TONE_CTX.load(Ordering::Acquire);
    if ctx.is_null() {
        return;
    }
    // SAFETY: `TONE_CTX` is set to a leaked `Box<ToneCtx>` before `ASIOStart` and
    // is never freed, so the pointee stays live for any in-flight callback.
    let ctx = unsafe { &*ctx };
    let half = (double_buffer_index & 1) as usize; // mask to 0/1, never panics
    let frames = ctx.frames;

    // SAFETY: `scratch` is accessed only here, on the single driver RT thread,
    // between `start` and `stop`; there is no other live reference to it.
    let scratch = unsafe { &mut *ctx.scratch.get() };
    let start_frame = ctx.frame_counter.load(Ordering::Relaxed);
    fill_tone(scratch, CHANNELS, start_frame, ctx.sample_rate);
    ctx.frame_counter
        .store(start_frame + frames as u64, Ordering::Relaxed);

    if first {
        // Record the generated signal range once, to confirm it is non-trivial.
        let (mut lo, mut hi) = (i32::MAX, i32::MIN);
        for &s in scratch.iter() {
            lo = lo.min(s);
            hi = hi.max(s);
        }
        DBG_SAMPLE_MIN.store(lo, Ordering::Relaxed);
        DBG_SAMPLE_MAX.store(hi, Ordering::Relaxed);
    }

    let bps = ctx.bps;
    for ch in 0..CHANNELS {
        let base = ctx.out_buffers[ch][half];
        if base.is_null() {
            if first {
                DBG_NULL_BUFFERS.fetch_or(1 << ch, Ordering::Relaxed);
            }
            continue;
        }
        // SAFETY: `createBuffers` allocated `frames * bps` writable bytes for this
        // channel/half; `base` is the address it reported. The two indexings below
        // stay within `frames * bps` and `frames * CHANNELS`, so neither panics.
        let out = unsafe { core::slice::from_raw_parts_mut(base, frames * bps) };
        for f in 0..frames {
            let sample = scratch[f * CHANNELS + ch];
            write_dst_sample(sample, 32, ctx.dst, &mut out[f * bps..f * bps + bps]);
        }
    }

    // SAFETY: `ctx.com` is the live `IASIO` pointer stored before `ASIOStart`;
    // `outputReady` is the one driver call the ASIO spec permits from `bufferSwitch`.
    unsafe { output_ready_raw(ctx.com) };
}

/// `sampleRateDidChange`: the driver's clock changed; logged only.
unsafe extern "system" fn tone_sample_rate_did_change(s_rate: AsioSampleRate) {
    crate::vprintln!("[ASIO] sampleRateDidChange: {s_rate} Hz");
}

/// `asioMessage`: the driver queries host capabilities. We advertise none of the
/// optional selectors (so the driver uses plain `bufferSwitch`) and report ASIO
/// engine version 2.
unsafe extern "system" fn tone_asio_message(
    selector: i32,
    _value: i32,
    _message: *mut c_void,
    _opt: *mut f64,
) -> i32 {
    const KASIO_SELECTOR_SUPPORTED: i32 = 1;
    const KASIO_ENGINE_VERSION: i32 = 2;
    match selector {
        KASIO_SELECTOR_SUPPORTED => 0, // no optional selectors advertised
        KASIO_ENGINE_VERSION => 2,
        _ => 0,
    }
}

/// `bufferSwitchTimeInfo`: only invoked if we advertised time-info support, which
/// we do not. Provided for ABI completeness; delegates defensively.
unsafe extern "system" fn tone_buffer_switch_time_info(
    params: *mut AsioTime,
    double_buffer_index: i32,
    direct_process: AsioBool,
) -> *mut AsioTime {
    // SAFETY: same RT-thread contract as `tone_buffer_switch`, which it forwards to.
    unsafe { tone_buffer_switch(double_buffer_index, direct_process) };
    params
}

/// Smoke test (`ASIO_TONE`): play a tone through the first driver, blocking the
/// calling thread for the duration.
pub(crate) fn run_tone_test(info: &AsioDriverInfo) {
    // SAFETY: a standard ASIO init -> createBuffers -> start -> stop -> dispose
    // sequence. `infos` and `callbacks` are locals that outlive `dispose_buffers`,
    // so the pointers the driver retains stay valid for the whole session.
    unsafe {
        let driver = match AsioDriver::create(info.clsid) {
            Ok(d) => d,
            Err(hr) => {
                crate::vprintln!("[ASIO] tone: '{}' create failed: hr={hr:#010x}", info.name);
                return;
            }
        };
        if driver.init(GetDesktopWindow()) == 0 {
            crate::vprintln!("[ASIO] tone: '{}' init failed", info.name);
            return;
        }

        // With `ASIO_PANEL` set, open the driver's own config dialog before we read
        // its capabilities, so a device/buffer change applies before `createBuffers`.
        // Blocks until the user closes the panel.
        if std::env::var_os("ASIO_PANEL").is_some() {
            crate::vprintln!("[ASIO] tone: opening control panel (close it to continue)");
            driver.control_panel();
        }

        let (mut num_in, mut num_out) = (0i32, 0i32);
        if !asio_ok(driver.get_channels(&mut num_in, &mut num_out)) {
            crate::vprintln!("[ASIO] tone: getChannels failed");
            return;
        }
        if num_out < CHANNELS as i32 {
            crate::vprintln!("[ASIO] tone: needs {CHANNELS} output channels, driver has {num_out}");
            return;
        }

        let (mut min, mut max, mut pref, mut gran) = (0i32, 0i32, 0i32, 0i32);
        if !asio_ok(driver.get_buffer_size(&mut min, &mut max, &mut pref, &mut gran)) {
            crate::vprintln!("[ASIO] tone: getBufferSize failed");
            return;
        }
        let frames = pref.max(1) as usize;

        let mut rate = 0.0f64;
        if !asio_ok(driver.get_sample_rate(&mut rate)) {
            crate::vprintln!("[ASIO] tone: getSampleRate failed");
            return;
        }

        // Output channel 0's sample type (assumed uniform across output channels).
        let mut ch_info = AsioChannelInfo {
            channel: 0,
            is_input: 0,
            is_active: 0,
            channel_group: 0,
            sample_type: 0,
            name: [0; 32],
        };
        if !asio_ok(driver.get_channel_info(&mut ch_info)) {
            crate::vprintln!("[ASIO] tone: getChannelInfo failed");
            return;
        }
        let dst = match AsioSampleType::from_asio(ch_info.sample_type) {
            Some(t) => t,
            None => {
                crate::vprintln!(
                    "[ASIO] tone: unsupported sample type {}",
                    ch_info.sample_type
                );
                return;
            }
        };
        let bps = dst.bytes_per_sample();

        // One `AsioBufferInfo` per output channel; `createBuffers` fills `buffers`.
        let mut infos = [
            AsioBufferInfo {
                is_input: 0,
                channel_num: 0,
                buffers: [core::ptr::null_mut(); 2],
            },
            AsioBufferInfo {
                is_input: 0,
                channel_num: 1,
                buffers: [core::ptr::null_mut(); 2],
            },
        ];
        let callbacks = AsioCallbacks {
            buffer_switch: tone_buffer_switch,
            sample_rate_did_change: tone_sample_rate_did_change,
            asio_message: tone_asio_message,
            buffer_switch_time_info: tone_buffer_switch_time_info,
        };
        if !asio_ok(driver.create_buffers(
            infos.as_mut_ptr(),
            CHANNELS as i32,
            frames as i32,
            &callbacks,
        )) {
            crate::vprintln!("[ASIO] tone: createBuffers failed");
            return;
        }

        let ctx = ToneCtx {
            frames,
            dst,
            bps,
            sample_rate: rate,
            out_buffers: [
                [
                    infos[0].buffers[0] as *mut u8,
                    infos[0].buffers[1] as *mut u8,
                ],
                [
                    infos[1].buffers[0] as *mut u8,
                    infos[1].buffers[1] as *mut u8,
                ],
            ],
            com: driver.as_ptr(),
            frame_counter: AtomicU64::new(0),
            scratch: UnsafeCell::new(vec![0i32; frames * CHANNELS].into_boxed_slice()),
        };
        // Leak the context: an in-flight callback may still deref it after we null the
        // pointer below. Smoke test runs once per launch; AsioHandle owns real teardown.
        TONE_CTX.store(Box::into_raw(Box::new(ctx)), Ordering::Release);

        crate::vprintln!(
            "[ASIO] tone: starting {CHANNELS}ch / {frames} frames / {rate} Hz / type {}",
            ch_info.sample_type
        );
        TONE_SWITCHES.store(0, Ordering::Relaxed);
        DBG_NULL_BUFFERS.store(0, Ordering::Relaxed);
        DBG_SAMPLE_MIN.store(0, Ordering::Relaxed);
        DBG_SAMPLE_MAX.store(0, Ordering::Relaxed);
        if !asio_ok(driver.start()) {
            crate::vprintln!("[ASIO] tone: start failed");
            TONE_CTX.store(core::ptr::null_mut(), Ordering::Release);
            driver.dispose_buffers();
            return;
        }
        crate::vprintln!("[ASIO] tone: start ok, playing 3s");

        std::thread::sleep(Duration::from_secs(3));

        driver.stop();
        TONE_CTX.store(core::ptr::null_mut(), Ordering::Release);
        driver.dispose_buffers();
        let switches = TONE_SWITCHES.load(Ordering::Relaxed);
        let expected = (rate * 3.0 / frames as f64) as u64;
        crate::vprintln!(
            "[ASIO] tone: done - bufferSwitch fired {switches} times (expected ~{expected})"
        );
        let null_bits = DBG_NULL_BUFFERS.load(Ordering::Relaxed);
        let smin = DBG_SAMPLE_MIN.load(Ordering::Relaxed);
        let smax = DBG_SAMPLE_MAX.load(Ordering::Relaxed);
        crate::vprintln!(
            "[ASIO] tone: diag - null_buffer_bits={null_bits} generated_range=[{smin}, {smax}]"
        );
        // `driver` drops here -> Release.
    }
}

// ---------------------------------------------------------------------------
// Streaming path: decoded PCM -> lock-free ring -> ASIO RT callback
// ---------------------------------------------------------------------------

/// Consumer-side state for the RT callback: `bufferSwitch` pulls interleaved i32
/// from the lock-free ring.
struct StreamCtx {
    frames: usize,
    channels: usize,
    dst: AsioSampleType,
    bps: usize,
    out_buffers: [[*mut u8; 2]; CHANNELS],
    /// Raw `IASIO` COM pointer, for `outputReady`.
    com: *mut c_void,
    /// Digital gain as f32 bits (1.0 = bit-perfect passthrough). Live so the
    /// control thread can drive it from the volume slider.
    gain: Arc<AtomicU32>,
    /// Lock-free ring consumer; touched only on the driver RT thread.
    consumer: UnsafeCell<rtrb::Consumer<i32>>,
    /// Pre-allocated interleaved-i32 scratch (`frames * channels`).
    scratch: UnsafeCell<Box<[i32]>>,
    /// Bumped by the control thread on seek/track-change; the RT callback drains
    /// the ring once when it changes so stale pre-seek audio is not played
    /// (mirrors the cpal `seek_gen` drain in `thread/output.rs`).
    flush_gen: Arc<AtomicU32>,
    /// The RT thread's last-seen flush generation (touched only on the RT thread).
    local_flush_gen: AtomicU32,
    /// Real frames the RT callback has consumed from the ring (excludes underrun
    /// zero-fill); the control thread reads it for position reporting.
    played_frames: Arc<AtomicU64>,
    /// Set on pause: the RT callback emits silence without consuming the ring or
    /// advancing position, so the driver clock stays running (device held).
    paused: Arc<AtomicBool>,
}

static STREAM_CTX: AtomicPtr<StreamCtx> = AtomicPtr::new(core::ptr::null_mut());

/// Diagnostic: RT-callback underruns (ring drained, tail zero-filled).
static STREAM_UNDERRUNS: AtomicU64 = AtomicU64::new(0);

/// `bufferSwitch` for the streaming path: pull `frames * channels` interleaved i32
/// from the ring (zero-fill on underrun), apply gain, write each channel into its
/// ASIO buffer half, signal `outputReady`. Runs on the driver's real-time thread.
unsafe extern "system" fn stream_buffer_switch(
    double_buffer_index: i32,
    _direct_process: AsioBool,
) {
    let ctx = STREAM_CTX.load(Ordering::Acquire);
    if ctx.is_null() {
        return;
    }
    // SAFETY: `STREAM_CTX` is a leaked `Box<StreamCtx>` set before `ASIOStart` and
    // never freed, so the pointee stays live for any in-flight callback.
    let ctx = unsafe { &*ctx };
    let half = (double_buffer_index & 1) as usize;
    let frames = ctx.frames;
    let channels = ctx.channels;
    let n = frames * channels;

    // SAFETY: `scratch` and `consumer` are touched only here, on the single driver
    // RT thread, between `start` and `stop`; no other live reference exists.
    let scratch = unsafe { &mut *ctx.scratch.get() };
    let consumer = unsafe { &mut *ctx.consumer.get() };

    // Flush-gen changed (seek / track-change): drain the ring once so stale
    // pre-seek audio is not played, then adopt the new generation. Mirrors the
    // cpal `seek_gen` drain in `thread/output.rs`.
    let cur_gen = ctx.flush_gen.load(Ordering::Relaxed);
    if cur_gen != ctx.local_flush_gen.load(Ordering::Relaxed) {
        let avail = consumer.slots();
        if avail > 0
            && let Ok(chunk) = consumer.read_chunk(avail)
        {
            chunk.commit_all();
        }
        ctx.local_flush_gen.store(cur_gen, Ordering::Relaxed);
    }

    if ctx.paused.load(Ordering::Relaxed) {
        // Paused: emit silence without consuming the ring (preserve it for instant
        // resume) or advancing position. The flush-gen drain above still ran.
        for s in &mut scratch[..n] {
            *s = 0;
        }
    } else {
        let to_read = consumer.slots().min(n);
        if to_read > 0
            && let Ok(chunk) = consumer.read_chunk(to_read)
        {
            let (s1, s2) = chunk.as_slices();
            scratch[..s1.len()].copy_from_slice(s1);
            scratch[s1.len()..to_read].copy_from_slice(s2);
            chunk.commit_all();
        }
        if to_read < n {
            for s in &mut scratch[to_read..n] {
                *s = 0; // underrun -> silence
            }
            STREAM_UNDERRUNS.fetch_add(1, Ordering::Relaxed);
        }
        // Count only the real frames consumed (not the underrun zero-fill), so the
        // control thread's position reporting tracks audible audio.
        ctx.played_frames
            .fetch_add((to_read / channels) as u64, Ordering::Relaxed);
    }

    let bps = ctx.bps;
    let gain = f32::from_bits(ctx.gain.load(Ordering::Relaxed));
    for ch in 0..channels {
        let base = ctx.out_buffers[ch][half];
        if base.is_null() {
            continue;
        }
        // SAFETY: `createBuffers` allocated `frames * bps` writable bytes for this
        // channel/half; both indexings stay within `frames * bps` / `frames * channels`.
        let out = unsafe { core::slice::from_raw_parts_mut(base, frames * bps) };
        for f in 0..frames {
            let sample = apply_gain(scratch[f * channels + ch], gain);
            write_dst_sample(sample, 32, ctx.dst, &mut out[f * bps..f * bps + bps]);
        }
    }

    // SAFETY: `ctx.com` is the live `IASIO` pointer; `outputReady` is permitted here.
    unsafe { output_ready_raw(ctx.com) };
}

/// Smoke test: decode `path` to interleaved i32 and feed `producer` with back-pressure;
/// report `(sample_rate, channels)` over `meta_tx` after probing.
fn decode_file_to_ring(
    path: std::ffi::OsString,
    mut producer: rtrb::Producer<i32>,
    cancel: Arc<AtomicBool>,
    meta_tx: mpsc::Sender<(u32, u32)>,
) {
    let file = match File::open(&path) {
        Ok(f) => f,
        Err(e) => {
            crate::vprintln!("[ASIO] stream: open failed: {e}");
            return;
        }
    };
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = std::path::Path::new(&path)
        .extension()
        .and_then(|e| e.to_str())
    {
        hint.with_extension(ext);
    }

    let mut format_reader = match symphonia::default::get_probe().probe(
        &hint,
        mss,
        FormatOptions::default(),
        MetadataOptions::default(),
    ) {
        Ok(r) => r,
        Err(e) => {
            crate::vprintln!("[ASIO] stream: probe failed: {e}");
            return;
        }
    };

    let Some(track) = format_reader
        .tracks()
        .iter()
        .find(|t| matches!(&t.codec_params, Some(CodecParameters::Audio(_))))
        .cloned()
    else {
        crate::vprintln!("[ASIO] stream: no audio track");
        return;
    };
    let codec_params = match &track.codec_params {
        Some(CodecParameters::Audio(p)) => p,
        _ => return,
    };
    let sample_rate = codec_params.sample_rate.unwrap_or(0);
    let channels = codec_params
        .channels
        .as_ref()
        .map(|c| c.count() as u32)
        .unwrap_or(0);
    if sample_rate == 0 || channels == 0 {
        crate::vprintln!("[ASIO] stream: missing sample rate / channels");
        return;
    }

    let mut decoder = match symphonia::default::get_codecs()
        .make_audio_decoder(codec_params, &AudioDecoderOptions::default())
    {
        Ok(d) => d,
        Err(e) => {
            crate::vprintln!("[ASIO] stream: decoder creation failed: {e}");
            return;
        }
    };
    let track_id = track.id;

    // Unblock the control thread now that the format is known.
    let _ = meta_tx.send((sample_rate, channels));

    let mut samples: Vec<i32> = Vec::new();
    loop {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        let packet = match format_reader.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) => break, // EOF
            Err(e) => {
                crate::vprintln!("[ASIO] stream: decode packet error: {e}");
                break;
            }
        };
        if packet.track_id != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(_) => continue,
        };
        samples.clear();
        decoded.copy_to_vec_interleaved::<i32>(&mut samples);

        // Back-pressure: spin briefly when the ring is full, bailing on cancel.
        let mut offset = 0;
        while offset < samples.len() {
            if cancel.load(Ordering::Relaxed) {
                return;
            }
            let available = producer.slots();
            if available == 0 {
                std::thread::sleep(Duration::from_millis(1));
                continue;
            }
            let to_write = (samples.len() - offset).min(available);
            if let Ok(chunk) = producer.write_chunk_uninit(to_write) {
                let written =
                    chunk.fill_from_iter(samples[offset..offset + to_write].iter().copied());
                offset += written;
            }
        }
    }
    crate::vprintln!("[ASIO] stream: decode finished");
}

/// Smoke test: play a producer-filled ring through ASIO until the producer finishes
/// (draining the ring) or `cap_secs` elapse.
fn run_ring_to_asio(
    info: &AsioDriverInfo,
    consumer: rtrb::Consumer<i32>,
    sample_rate: u32,
    channels: usize,
    producer_thread: std::thread::JoinHandle<()>,
    cancel: Arc<AtomicBool>,
    cap_secs: u64,
) {
    let stop_producer = |cancel: &Arc<AtomicBool>| cancel.store(true, Ordering::Relaxed);

    // SAFETY: a standard ASIO init -> createBuffers -> start -> stop -> dispose
    // sequence; `infos`/`callbacks` are locals that outlive `dispose_buffers`.
    unsafe {
        let driver = match AsioDriver::create(info.clsid) {
            Ok(d) => d,
            Err(hr) => {
                crate::vprintln!(
                    "[ASIO] stream: '{}' create failed: hr={hr:#010x}",
                    info.name
                );
                stop_producer(&cancel);
                let _ = producer_thread.join();
                return;
            }
        };
        if driver.init(GetDesktopWindow()) == 0 {
            crate::vprintln!("[ASIO] stream: '{}' init failed", info.name);
            stop_producer(&cancel);
            let _ = producer_thread.join();
            return;
        }
        if std::env::var_os("ASIO_PANEL").is_some() {
            crate::vprintln!("[ASIO] stream: opening control panel (close it to continue)");
            driver.control_panel();
        }

        // Match the driver clock to the file (mirror the real rate-follow).
        if asio_ok(driver.can_sample_rate(sample_rate as f64)) {
            driver.set_sample_rate(sample_rate as f64);
        } else {
            crate::vprintln!(
                "[ASIO] stream: driver rejects {sample_rate} Hz, using current rate (pitch may differ)"
            );
        }

        let (mut num_in, mut num_out) = (0i32, 0i32);
        if !asio_ok(driver.get_channels(&mut num_in, &mut num_out)) {
            crate::vprintln!("[ASIO] stream: getChannels failed");
            stop_producer(&cancel);
            let _ = producer_thread.join();
            return;
        }
        if num_out < channels as i32 {
            crate::vprintln!(
                "[ASIO] stream: needs {channels} output channels, driver has {num_out}"
            );
            stop_producer(&cancel);
            let _ = producer_thread.join();
            return;
        }

        let (mut min, mut max, mut pref, mut gran) = (0i32, 0i32, 0i32, 0i32);
        if !asio_ok(driver.get_buffer_size(&mut min, &mut max, &mut pref, &mut gran)) {
            crate::vprintln!("[ASIO] stream: getBufferSize failed");
            stop_producer(&cancel);
            let _ = producer_thread.join();
            return;
        }
        let frames = pref.max(1) as usize;

        let mut ch_info = AsioChannelInfo {
            channel: 0,
            is_input: 0,
            is_active: 0,
            channel_group: 0,
            sample_type: 0,
            name: [0; 32],
        };
        if !asio_ok(driver.get_channel_info(&mut ch_info)) {
            crate::vprintln!("[ASIO] stream: getChannelInfo failed");
            stop_producer(&cancel);
            let _ = producer_thread.join();
            return;
        }
        let dst = match AsioSampleType::from_asio(ch_info.sample_type) {
            Some(t) => t,
            None => {
                crate::vprintln!(
                    "[ASIO] stream: unsupported sample type {}",
                    ch_info.sample_type
                );
                stop_producer(&cancel);
                let _ = producer_thread.join();
                return;
            }
        };
        let bps = dst.bytes_per_sample();

        let mut infos: Vec<AsioBufferInfo> = (0..channels)
            .map(|ch| AsioBufferInfo {
                is_input: 0,
                channel_num: ch as i32,
                buffers: [core::ptr::null_mut(); 2],
            })
            .collect();
        let callbacks = AsioCallbacks {
            buffer_switch: stream_buffer_switch,
            sample_rate_did_change: tone_sample_rate_did_change,
            asio_message: tone_asio_message,
            buffer_switch_time_info: tone_buffer_switch_time_info,
        };
        if !asio_ok(driver.create_buffers(
            infos.as_mut_ptr(),
            channels as i32,
            frames as i32,
            &callbacks,
        )) {
            crate::vprintln!("[ASIO] stream: createBuffers failed");
            stop_producer(&cancel);
            let _ = producer_thread.join();
            return;
        }

        let mut out_buffers = [[core::ptr::null_mut::<u8>(); 2]; CHANNELS];
        for ch in 0..channels {
            out_buffers[ch] = [
                infos[ch].buffers[0] as *mut u8,
                infos[ch].buffers[1] as *mut u8,
            ];
        }

        // Pre-fill a cushion so the first callbacks do not underrun.
        let cushion = frames * channels * 4;
        for _ in 0..1000 {
            if consumer.slots() >= cushion {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        crate::vprintln!(
            "[ASIO] stream: pre-filled {} samples (cushion {cushion})",
            consumer.slots()
        );

        let ctx = StreamCtx {
            frames,
            channels,
            dst,
            bps,
            out_buffers,
            com: driver.as_ptr(),
            gain: Arc::new(AtomicU32::new(f32::to_bits(1.0))),
            consumer: UnsafeCell::new(consumer),
            scratch: UnsafeCell::new(vec![0i32; frames * channels].into_boxed_slice()),
            flush_gen: Arc::new(AtomicU32::new(0)),
            local_flush_gen: AtomicU32::new(0),
            played_frames: Arc::new(AtomicU64::new(0)),
            paused: Arc::new(AtomicBool::new(false)),
        };
        // Leak the context (smoke test runs once; the real AsioHandle owns teardown).
        STREAM_CTX.store(Box::into_raw(Box::new(ctx)), Ordering::Release);

        let mut rate = 0.0f64;
        let _ = driver.get_sample_rate(&mut rate);
        crate::vprintln!(
            "[ASIO] stream: starting {channels}ch / {frames} frames / {rate} Hz / type {}",
            ch_info.sample_type
        );
        if !asio_ok(driver.start()) {
            crate::vprintln!("[ASIO] stream: start failed");
            STREAM_CTX.store(core::ptr::null_mut(), Ordering::Release);
            stop_producer(&cancel);
            driver.dispose_buffers();
            let _ = producer_thread.join();
            return;
        }
        crate::vprintln!("[ASIO] stream: playing (up to {cap_secs} s)");

        // Play until the producer finishes (then let the ring drain) or cap_secs elapse.
        let start = Instant::now();
        loop {
            std::thread::sleep(Duration::from_millis(100));
            if producer_thread.is_finished() {
                std::thread::sleep(Duration::from_millis(1500));
                break;
            }
            if start.elapsed() > Duration::from_secs(cap_secs) {
                break;
            }
        }

        driver.stop();
        STREAM_CTX.store(core::ptr::null_mut(), Ordering::Release);
        stop_producer(&cancel);
        driver.dispose_buffers();
        let _ = producer_thread.join();
        crate::vprintln!("[ASIO] stream: done");
    }
}

/// Decode a local audio file through the ring into ASIO. Gated `ASIO_FILE=<path>`.
pub(crate) fn run_flac_test(info: &AsioDriverInfo, path: std::ffi::OsString) {
    const MAX_RING: usize = 192_000 * 2 * 2; // ~2 s at 192 kHz stereo
    let (producer, consumer) = rtrb::RingBuffer::<i32>::new(MAX_RING);
    let cancel = Arc::new(AtomicBool::new(false));
    let (meta_tx, meta_rx) = mpsc::channel::<(u32, u32)>();
    let cancel_producer = cancel.clone();
    let producer_thread =
        std::thread::spawn(move || decode_file_to_ring(path, producer, cancel_producer, meta_tx));
    let (sample_rate, file_channels) = match meta_rx.recv() {
        Ok(m) => m,
        Err(_) => {
            crate::vprintln!("[ASIO] file: probe failed (producer exited)");
            let _ = producer_thread.join();
            return;
        }
    };
    let channels = (file_channels as usize).clamp(1, CHANNELS);
    crate::vprintln!("[ASIO] file: {sample_rate} Hz / {file_channels}ch (using {channels})");
    run_ring_to_asio(
        info,
        consumer,
        sample_rate,
        channels,
        producer_thread,
        cancel,
        60,
    );
}

/// Smoke test: generate a test tone into the ring with the same back-pressure as
/// the decode producer, until cancelled.
fn tone_to_ring(
    mut producer: rtrb::Producer<i32>,
    cancel: Arc<AtomicBool>,
    sample_rate: u32,
    channels: usize,
) {
    const FREQ: f64 = 440.0;
    const AMP: f64 = 0.2 * i32::MAX as f64; // ~-14 dBFS, headphone-safe
    const BLOCK: usize = 1024; // frames generated per chunk
    let mut frame: u64 = 0;
    let mut samples: Vec<i32> = Vec::with_capacity(BLOCK * channels);
    loop {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        samples.clear();
        for f in 0..BLOCK {
            let n = frame + f as u64;
            let phase = core::f64::consts::TAU * (n as f64) * FREQ / sample_rate as f64;
            let s = (AMP * phase.sin()) as i32;
            for _ in 0..channels {
                samples.push(s);
            }
        }
        frame += BLOCK as u64;
        let mut offset = 0;
        while offset < samples.len() {
            if cancel.load(Ordering::Relaxed) {
                return;
            }
            let available = producer.slots();
            if available == 0 {
                std::thread::sleep(Duration::from_millis(1));
                continue;
            }
            let to_write = (samples.len() - offset).min(available);
            if let Ok(chunk) = producer.write_chunk_uninit(to_write) {
                let written =
                    chunk.fill_from_iter(samples[offset..offset + to_write].iter().copied());
                offset += written;
            }
        }
    }
}

/// Play a 440 Hz tone through the ring into ASIO (no input file needed). Gated
/// `ASIO_RING`; validates the ring producer/consumer/back-pressure/underrun path.
pub(crate) fn run_ring_tone_test(info: &AsioDriverInfo) {
    const MAX_RING: usize = 48_000 * 2 * 2; // ~2 s at 48 kHz stereo
    let sample_rate = 44_100u32;
    let channels = CHANNELS;
    let (producer, consumer) = rtrb::RingBuffer::<i32>::new(MAX_RING);
    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_producer = cancel.clone();
    let producer_thread =
        std::thread::spawn(move || tone_to_ring(producer, cancel_producer, sample_rate, channels));
    crate::vprintln!("[ASIO] ring: 440 Hz tone through ring, {sample_rate} Hz / {channels}ch");
    run_ring_to_asio(
        info,
        consumer,
        sample_rate,
        channels,
        producer_thread,
        cancel,
        5,
    );
}

// ---------------------------------------------------------------------------
// The player-integrated AsioHandle.
//
// Mirrors `wasapi::ExclusiveHandle`. A control thread owns the `AsioDriver` and
// the `rtrb::Producer<i32>`; the driver's RT `stream_buffer_switch` owns the
// `Consumer` via `STREAM_CTX`. Decode threads feed audio as `AsioCommand::PushPcm`
// (a cloneable mpsc) because rtrb is SPSC - the control thread is the sole writer.
// ---------------------------------------------------------------------------

/// Commands into the ASIO control thread. `PushPcm` carries interleaved i32
/// (MSB-justified, from `copy_to_vec_interleaved::<i32>`) - the ring is i32, so
/// no byte-packing step (unlike the WASAPI `PushPcm`).
pub(crate) enum AsioCommand {
    StartStream {
        stream_id: u32,
        sample_rate: u32,
        channels: u32,
        duration_secs: f64,
        start_secs: f64,
        start_paused: bool,
    },
    PushPcm {
        stream_id: u32,
        samples: Vec<i32>,
    },
    EndStream {
        stream_id: u32,
    },
    /// In-place seek: flush the ring + staging and re-base the reported position,
    /// without reopening the driver.
    ResetForSeek {
        stream_id: u32,
        start_secs: f64,
    },
    Play,
    Pause,
    Stop,
    Shutdown,
}

/// Events out of the ASIO control thread. `DriverNotFound`/`FormatUnsupported`/
/// `InitFailed` map to the matching `DeviceErrorKind::Asio*` + a Shared fallback.
pub(crate) enum AsioEvent {
    TimeUpdate(f64),
    StateChange(PlaybackState),
    Duration(f64),
    DriverNotFound,
    FormatUnsupported,
    InitFailed(String),
    /// Track finished (EOF + drained). Carries the stream_id so a stale completion from a
    /// superseded stream can't clear a newer track (which would force a re-arm/double-load).
    Completed(u32),
    /// Clock stopped. Carries the stream_id for the same stale-event guard.
    Stopped(u32),
}

/// Absolute frame index for a position in seconds, the `played_frames` baseline so
/// reporting stays correct after a seek/resume. Mirrors `wasapi::position_frames`.
fn position_frames(secs: f64, sample_rate: u32) -> u64 {
    if sample_rate > 0 && secs.is_finite() && secs > 0.0 {
        (secs * sample_rate as f64) as u64
    } else {
        0
    }
}

enum AsioState {
    Idle,
    Playing,
    Paused,
}

/// Resources produced by `ControlCtx::build_stream` once the driver buffers are
/// created and a fresh `StreamCtx` is allocated for the RT callback.
struct OpenedAsioStream {
    ctx_ptr: *mut StreamCtx,
    producer: rtrb::Producer<i32>,
    ring_capacity: usize,
    frames: usize,
    flush_gen: Arc<AtomicU32>,
    played_frames: Arc<AtomicU64>,
    paused: Arc<AtomicBool>,
}

/// State owned by the ASIO control thread. Never crosses threads (created and used
/// only on that thread), so it needs no `Send`.
struct ControlCtx {
    /// Shared digital gain (f32 bits), cloned into each `StreamCtx`.
    gain: Arc<AtomicU32>,
    /// Samples currently buffered (ring + staging), published for the decode thread
    /// to back-pressure on - so it reads at ~real time instead of flooding a
    /// streaming source far past its download window (which stalls the read).
    buffered: Arc<AtomicU64>,
    driver: Option<AsioDriver>,
    /// The single ring writer (the RT callback holds the matching `Consumer`).
    producer: Option<rtrb::Producer<i32>>,
    ring_capacity: usize,
    /// Decoded i32 not yet written to the ring (absorbs decode-ahead, like
    /// wasapi's `pcm_data`).
    staging: VecDeque<i32>,
    /// Shared with the live `StreamCtx`: bumped to drain the ring on seek/track-change.
    flush_gen: Arc<AtomicU32>,
    /// Shared with the live `StreamCtx`: real frames the RT callback has consumed.
    played_frames: Arc<AtomicU64>,
    /// Shared with the live `StreamCtx`: while true the RT callback emits silence without
    /// consuming the ring, so the driver clock stays held (device kept) during a pause.
    paused: Arc<AtomicBool>,
    /// Heap-stable callbacks struct handed to `createBuffers` (kept alive for the
    /// driver's lifetime).
    callbacks: Box<AsioCallbacks>,
    stream_id: Option<u32>,
    sample_rate: u32,
    channels: usize,
    frames: usize,
    duration: f64,
    /// Frame index of the stream start (from `start_secs`).
    baseline_frames: u64,
    /// `played_frames` snapshot at the last start/seek, subtracted so position is
    /// relative to this stream rather than the lifetime of the `played_frames` cell.
    played_offset: u64,
    stream_ended: bool,
    pending_start: bool,
    /// When the deferred (cold) start began waiting for a cushion; bounds the wait
    /// so a slow/stalled decode can't leave ASIO silent forever.
    pending_since: Option<Instant>,
    client_started: bool,
    has_buffers: bool,
    /// Set when the ring first drains after EOF; completion fires after a short
    /// grace so the last ASIO double-buffer plays out.
    drain_since: Option<Instant>,
    last_time_report: Instant,
    /// Throttle for the diagnostic status log.
    last_status: Instant,
    /// The leaked-then-reclaimed `StreamCtx` pointer (null when no stream is open).
    ctx_ptr: *mut StreamCtx,
    state: AsioState,
}

impl ControlCtx {
    fn new(gain: Arc<AtomicU32>, buffered: Arc<AtomicU64>) -> Self {
        Self {
            gain,
            buffered,
            driver: None,
            producer: None,
            ring_capacity: 0,
            staging: VecDeque::new(),
            flush_gen: Arc::new(AtomicU32::new(0)),
            played_frames: Arc::new(AtomicU64::new(0)),
            paused: Arc::new(AtomicBool::new(false)),
            callbacks: Box::new(AsioCallbacks {
                buffer_switch: stream_buffer_switch,
                sample_rate_did_change: tone_sample_rate_did_change,
                asio_message: tone_asio_message,
                buffer_switch_time_info: tone_buffer_switch_time_info,
            }),
            stream_id: None,
            sample_rate: 0,
            channels: 0,
            frames: 0,
            duration: 0.0,
            baseline_frames: 0,
            played_offset: 0,
            stream_ended: false,
            pending_start: false,
            pending_since: None,
            client_started: false,
            has_buffers: false,
            drain_since: None,
            last_time_report: Instant::now(),
            last_status: Instant::now(),
            ctx_ptr: core::ptr::null_mut(),
            state: AsioState::Idle,
        }
    }

    /// Open ASIO buffers on the current driver, install the RT callback + a fresh
    /// `StreamCtx`, and return the producer side of its ring. The driver must
    /// already be created and `init`'d.
    fn build_stream(
        &self,
        sample_rate: u32,
        channels: usize,
    ) -> Result<OpenedAsioStream, AsioEvent> {
        let Some(driver) = self.driver.as_ref() else {
            return Err(AsioEvent::InitFailed("no driver".into()));
        };
        // SAFETY: `driver` is a live, initialised IASIO; the calls below follow the
        // documented query -> createBuffers sequence. `self.callbacks` is heap-stable
        // and outlives the stream (dropped only with the handle).
        unsafe {
            if !asio_ok(driver.can_sample_rate(sample_rate as f64)) {
                return Err(AsioEvent::FormatUnsupported);
            }
            // Some clock-locked interfaces accept can_sample_rate but reject the switch;
            // rendering at the old clock plays wrong-speed, so fall back to shared.
            if !asio_ok(driver.set_sample_rate(sample_rate as f64)) {
                return Err(AsioEvent::FormatUnsupported);
            }

            let (mut num_in, mut num_out) = (0i32, 0i32);
            if !asio_ok(driver.get_channels(&mut num_in, &mut num_out)) {
                return Err(AsioEvent::InitFailed("getChannels failed".into()));
            }
            if num_out < channels as i32 {
                return Err(AsioEvent::FormatUnsupported);
            }

            let (mut min, mut max, mut pref, mut gran) = (0i32, 0i32, 0i32, 0i32);
            if !asio_ok(driver.get_buffer_size(&mut min, &mut max, &mut pref, &mut gran)) {
                return Err(AsioEvent::InitFailed("getBufferSize failed".into()));
            }
            let frames = pref.max(1) as usize;

            let mut ch_info = AsioChannelInfo {
                channel: 0,
                is_input: 0,
                is_active: 0,
                channel_group: 0,
                sample_type: 0,
                name: [0; 32],
            };
            if !asio_ok(driver.get_channel_info(&mut ch_info)) {
                return Err(AsioEvent::InitFailed("getChannelInfo failed".into()));
            }
            let Some(dst) = AsioSampleType::from_asio(ch_info.sample_type) else {
                return Err(AsioEvent::FormatUnsupported);
            };
            let bps = dst.bytes_per_sample();

            let mut infos: Vec<AsioBufferInfo> = (0..channels)
                .map(|ch| AsioBufferInfo {
                    is_input: 0,
                    channel_num: ch as i32,
                    buffers: [core::ptr::null_mut(); 2],
                })
                .collect();
            if !asio_ok(driver.create_buffers(
                infos.as_mut_ptr(),
                channels as i32,
                frames as i32,
                &*self.callbacks,
            )) {
                return Err(AsioEvent::InitFailed("createBuffers failed".into()));
            }

            let mut out_buffers = [[core::ptr::null_mut::<u8>(); 2]; CHANNELS];
            for ch in 0..channels {
                out_buffers[ch] = [
                    infos[ch].buffers[0] as *mut u8,
                    infos[ch].buffers[1] as *mut u8,
                ];
            }

            // ~2 s ring, at least 8 ASIO buffers so the pre-fill cushion always fits.
            let ring_capacity = (sample_rate as usize * channels * 2).max(frames * channels * 8);
            let (producer, consumer) = rtrb::RingBuffer::<i32>::new(ring_capacity);
            let flush_gen = Arc::new(AtomicU32::new(0));
            let played_frames = Arc::new(AtomicU64::new(0));
            let paused = Arc::new(AtomicBool::new(false));

            let stream_ctx = StreamCtx {
                frames,
                channels,
                dst,
                bps,
                out_buffers,
                com: driver.as_ptr(),
                gain: self.gain.clone(),
                consumer: UnsafeCell::new(consumer),
                scratch: UnsafeCell::new(vec![0i32; frames * channels].into_boxed_slice()),
                flush_gen: flush_gen.clone(),
                local_flush_gen: AtomicU32::new(0),
                played_frames: played_frames.clone(),
                paused: paused.clone(),
            };
            Ok(OpenedAsioStream {
                ctx_ptr: Box::into_raw(Box::new(stream_ctx)),
                producer,
                ring_capacity,
                frames,
                flush_gen,
                played_frames,
                paused,
            })
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn handle_start_stream(
        &mut self,
        event_tx: &mpsc::Sender<AsioEvent>,
        stream_id: u32,
        sample_rate: u32,
        channels: u32,
        duration_secs: f64,
        start_secs: f64,
        start_paused: bool,
    ) -> Result<(), ()> {
        let want_channels = (channels as usize).clamp(1, CHANNELS);

        // First stream: open the first enumerated ASIO driver; it persists across
        // same-format tracks.
        if self.driver.is_none() {
            let Some(info) = enumerate_asio_drivers().into_iter().next() else {
                let _ = event_tx.send(AsioEvent::DriverNotFound);
                return Err(());
            };
            crate::vprintln!("[ASIO] control: opening driver '{}'", info.name);
            // SAFETY: standard COM create + init; the desktop window parents the panel.
            let driver = match unsafe { AsioDriver::create(info.clsid) } {
                Ok(d) => d,
                Err(hr) => {
                    let _ =
                        event_tx.send(AsioEvent::InitFailed(format!("create failed: {hr:#010x}")));
                    return Err(());
                }
            };
            if unsafe { driver.init(GetDesktopWindow()) } == 0 {
                let _ = event_tx.send(AsioEvent::InitFailed("driver init failed".into()));
                return Err(());
            }
            self.driver = Some(driver);
            self.has_buffers = false;
        }

        let format_changed =
            self.has_buffers && (self.sample_rate != sample_rate || self.channels != want_channels);

        if !self.has_buffers || format_changed {
            // (Re)create buffers: stop + dispose the old stream, then open fresh.
            self.dispose_stream();
            match self.build_stream(sample_rate, want_channels) {
                Ok(opened) => {
                    STREAM_CTX.store(opened.ctx_ptr, Ordering::Release);
                    self.ctx_ptr = opened.ctx_ptr;
                    self.producer = Some(opened.producer);
                    self.ring_capacity = opened.ring_capacity;
                    self.frames = opened.frames;
                    self.flush_gen = opened.flush_gen;
                    self.played_frames = opened.played_frames;
                    self.paused = opened.paused;
                    self.has_buffers = true;
                    // Fresh buffers: the clock is stopped (dispose_stream stopped it), so arm
                    // the deferred cold start. On a REUSE track-change the clock keeps running
                    // (continuous device hold), so these are NOT reset there -- doing so would
                    // make maybe_start issue a second driver.start() on the live driver.
                    self.client_started = false;
                    self.pending_start = true;
                    self.pending_since = Some(Instant::now());
                }
                Err(ev) => {
                    let _ = event_tx.send(ev);
                    return Err(());
                }
            }
        } else {
            // Reuse the open buffers -- the clock stays RUNNING (continuous device hold).
            // Bump flush_gen so the RT callback drains the previous track's audio from the
            // ring on its next tick. The control thread must NOT touch the consumer here (the
            // RT thread owns it while the clock runs); pump_ring is gated on the RT having
            // drained (local_flush_gen == flush_gen) so the new track's head isn't lost.
            self.flush_gen.fetch_add(1, Ordering::Relaxed);
            // A Stop during cold-start pre-fill leaves reusable buffers but an idle driver.
            // Re-arm the deferred cold start, guarded on !client_started so the normal
            // reuse-while-running path can't double-start the live clock.
            if !self.client_started {
                self.pending_start = true;
                self.pending_since = Some(Instant::now());
            }
        }

        self.staging.clear();
        // Reset the throttle counter so a new stream's decoder doesn't inherit the prior
        // track's `buffered` value and throttle-stall on its first packet.
        self.buffered.store(0, Ordering::Relaxed);
        self.stream_id = Some(stream_id);
        crate::vprintln3!(
            "[ASIO-DIAG] StartStream adopted: stream={stream_id} paused={start_paused} start={start_secs:.1}s sr={sample_rate} ch={want_channels}"
        );
        self.sample_rate = sample_rate;
        self.channels = want_channels;
        self.duration = duration_secs;
        self.baseline_frames = position_frames(start_secs, sample_rate);
        self.played_offset = self.played_frames.load(Ordering::Relaxed);
        self.stream_ended = false;
        self.drain_since = None;
        self.last_time_report = Instant::now();
        // Arm play/pause WITHOUT touching the clock (continuous hold): the RT emits silence
        // while paused and consumes once cleared. The cold-start flags are set only in the
        // rebuild branch above; on a REUSE track-change the clock is already running.
        self.paused.store(start_paused, Ordering::Relaxed);

        let _ = event_tx.send(AsioEvent::Duration(duration_secs));
        if start_paused {
            self.state = AsioState::Paused;
            let _ = event_tx.send(AsioEvent::StateChange(PlaybackState::Paused));
        } else {
            self.state = AsioState::Playing;
            let _ = event_tx.send(AsioEvent::StateChange(PlaybackState::Active));
        }
        Ok(())
    }

    fn handle_push_pcm(&mut self, stream_id: u32, samples: Vec<i32>) {
        if self.stream_id == Some(stream_id) {
            self.staging.extend(samples);
        } else {
            // A live decoder's PCM dropped (id != adopted stream) -> staging stays
            // empty -> ring starves to silence.
            crate::vprintln3!(
                "[ASIO-DIAG] PushPcm DROPPED: push_stream={stream_id} cur_stream={:?} ({} samples)",
                self.stream_id,
                samples.len(),
            );
        }
    }

    fn handle_end_stream(&mut self, stream_id: u32) {
        if self.stream_id == Some(stream_id) {
            self.stream_ended = true;
        }
    }

    fn handle_reset_for_seek(&mut self, stream_id: u32, start_secs: f64) {
        if self.stream_id != Some(stream_id) {
            return;
        }
        // Re-base the position and flush stale pre-seek audio. While the clock runs the RT
        // callback drains the ring on the flush_gen bump; while it is stopped (cold/paused) the
        // RT never ticks, so the ring is rebuilt below to evict any already-pumped pre-seek PCM.
        self.flush_gen.fetch_add(1, Ordering::Relaxed);
        crate::vprintln3!(
            "[ASIO-DIAG] reset_for_seek: stream={stream_id} start={start_secs:.1}s flush_gen={} staging(before clear)={} baseline->{} played_off->{}",
            self.flush_gen.load(Ordering::Relaxed),
            self.staging.len(),
            position_frames(start_secs, self.sample_rate),
            self.played_frames.load(Ordering::Relaxed),
        );
        self.staging.clear();
        self.baseline_frames = position_frames(start_secs, self.sample_rate);
        self.played_offset = self.played_frames.load(Ordering::Relaxed);
        self.stream_ended = false;
        self.drain_since = None;

        // Clock stopped: the RT can't drain, so rebuild the ring to evict any pre-seek PCM
        // already pumped into it, then adopt the flush generation on the RT side (else the
        // resume/maybe_start reconcile would mark the stale ring as seen and play it).
        if !self.client_started && !self.ctx_ptr.is_null() {
            let (producer, consumer) = rtrb::RingBuffer::<i32>::new(self.ring_capacity);
            self.producer = Some(producer);
            // SAFETY: client_started == false means the driver clock was never started, so no
            // RT callback is touching the consumer; safe to replace it and the local flush gen.
            unsafe {
                *(*self.ctx_ptr).consumer.get() = consumer;
                (*self.ctx_ptr)
                    .local_flush_gen
                    .store(self.flush_gen.load(Ordering::Relaxed), Ordering::Relaxed);
            }
        }
    }

    fn handle_stop(&mut self, event_tx: &mpsc::Sender<AsioEvent>) {
        // Keep the clock RUNNING (continuous device hold); just flush the ring + staging so
        // the RT emits silence until the next StartStream. The device is released only on
        // ASIO-off / shutdown (dispose_stream).
        self.flush_gen.fetch_add(1, Ordering::Relaxed);
        self.staging.clear();
        self.paused.store(false, Ordering::Relaxed);
        let stopped_id = self.stream_id.unwrap_or(0);
        crate::vprintln3!("[ASIO-DIAG] handle_stop: clearing stream={stopped_id} -> Idle");
        self.stream_id = None;
        self.stream_ended = false;
        self.pending_start = false;
        self.drain_since = None;
        self.state = AsioState::Idle;
        let _ = event_tx.send(AsioEvent::Stopped(stopped_id));
    }

    fn resume(&mut self, event_tx: &mpsc::Sender<AsioEvent>) {
        if !self.has_buffers {
            return;
        }
        self.state = AsioState::Playing;
        // Resume-from-pause finds the clock already running (continuous hold) and just clears
        // `paused` here; only a cold start (first play) reaches driver.start() below.
        self.paused.store(false, Ordering::Relaxed);
        // Restart the clock directly (NOT via maybe_start's cushion wait): the ring
        // still holds pre-pause data, and even if a seek/slow decode left it low, a
        // brief underrun beats resuming into permanent silence waiting on a cushion
        // that may never fill.
        if !self.client_started {
            self.pending_start = false;
            self.pending_since = None;
            let ring_used = self
                .producer
                .as_ref()
                .map_or(0, |p| self.ring_capacity.saturating_sub(p.slots()));
            crate::vprintln3!(
                "[ASIO-DIAG] resume direct-start: ring={ring_used}/{} staging={} baseline={} played_off={}",
                self.ring_capacity,
                self.staging.len(),
                self.baseline_frames,
                self.played_offset,
            );
            // Adopt the current flush generation on the RT side before starting the clock: a
            // flush_gen bump from a paused initial-seek was never seen by the (stopped) RT, so
            // the first callback would drain the ring -- which here holds the correct post-seek
            // audio, not stale pre-seek audio.
            if !self.ctx_ptr.is_null() {
                let cur_gen = self.flush_gen.load(Ordering::Relaxed);
                // SAFETY: ctx_ptr is the leaked StreamCtx, live until dispose_stream; the
                // clock is stopped here (client_started==false) so no RT callback races this.
                unsafe {
                    (*self.ctx_ptr)
                        .local_flush_gen
                        .store(cur_gen, Ordering::Relaxed);
                }
            }
            if let Some(ref driver) = self.driver {
                // SAFETY: live driver with buffers created; ASIOStart restarts the clock.
                if !asio_ok(unsafe { driver.start() }) {
                    let _ =
                        event_tx.send(AsioEvent::InitFailed("ASIOStart (resume) failed".into()));
                    self.dispose_stream();
                    self.state = AsioState::Idle;
                    return;
                }
            }
            self.client_started = true;
            self.last_time_report = Instant::now();
            crate::vprintln!("[ASIO] control: resumed");
        }
        let _ = event_tx.send(AsioEvent::StateChange(PlaybackState::Active));
    }

    /// Write staged samples into the ring until the ring is full or staging empties.
    fn pump_ring(&mut self) {
        // While the clock is running and an RT flush is still pending, do NOT write: the RT
        // callback drains the ring once when it observes a flush_gen bump (track-change /
        // seek) then sets local_flush_gen. Writing the new track's head before that drain
        // would feed it into the drain and lose it. Gated on `client_started` so a paused
        // cold-start load (clock not yet started, RT never advances local_flush_gen) still
        // fills the ring -- the resume reconcile prevents that ring from being drained.
        if self.client_started && !self.ctx_ptr.is_null() {
            let want = self.flush_gen.load(Ordering::Relaxed);
            // SAFETY: ctx_ptr is the live leaked StreamCtx (valid until dispose_stream);
            // local_flush_gen is written only by the RT thread, read here (Relaxed).
            let seen = unsafe { (*self.ctx_ptr).local_flush_gen.load(Ordering::Relaxed) };
            if seen != want {
                return;
            }
        }
        let Some(producer) = self.producer.as_mut() else {
            return;
        };
        while !self.staging.is_empty() {
            let free = producer.slots();
            if free == 0 {
                break;
            }
            let n = free.min(self.staging.len());
            let Ok(chunk) = producer.write_chunk_uninit(n) else {
                break;
            };
            let written = chunk.fill_from_iter(self.staging.iter().copied().take(n));
            self.staging.drain(..written);
            if written == 0 {
                break;
            }
        }
    }

    /// Once the pre-fill cushion is buffered, start the driver clock (deferred so the
    /// first callbacks do not underrun - the ASIO analogue of wasapi's deferred Start).
    fn maybe_start(&mut self, event_tx: &mpsc::Sender<AsioEvent>) {
        if !self.pending_start || self.driver.is_none() {
            return;
        }
        let buffered = self
            .producer
            .as_ref()
            .map_or(0, |p| self.ring_capacity.saturating_sub(p.slots()));
        let cushion = self.frames * self.channels * 4;
        // Bound the cushion wait: a slow/stalled decode (network) must not leave ASIO
        // silent forever. After the timeout, start anyway and accept a brief underrun.
        let timed_out = self
            .pending_since
            .is_some_and(|t| t.elapsed() > Duration::from_millis(750));
        let ready =
            buffered >= cushion || (self.stream_ended && self.staging.is_empty()) || timed_out;
        if !ready {
            return;
        }
        // Same reconcile as `resume`: an auto-play cold start with an initial seek bumped
        // flush_gen while the clock was stopped, so adopt it before starting or the first
        // callback would drain the (correct) post-seek cushion.
        if !self.ctx_ptr.is_null() {
            let cur_gen = self.flush_gen.load(Ordering::Relaxed);
            // SAFETY: ctx_ptr is the leaked StreamCtx, live until dispose_stream; the clock is
            // stopped here (pending_start, client not started) so no RT callback races this.
            unsafe {
                (*self.ctx_ptr)
                    .local_flush_gen
                    .store(cur_gen, Ordering::Relaxed);
            }
        }
        if let Some(ref driver) = self.driver {
            // SAFETY: createBuffers installed our callbacks + STREAM_CTX, and the ring
            // holds a pre-fill cushion (or we timed out waiting for one).
            if !asio_ok(unsafe { driver.start() }) {
                let _ = event_tx.send(AsioEvent::InitFailed("ASIOStart failed".into()));
                self.dispose_stream();
                self.pending_start = false;
                self.pending_since = None;
                self.state = AsioState::Idle;
                return;
            }
        }
        self.client_started = true;
        self.pending_start = false;
        self.pending_since = None;
        self.last_time_report = Instant::now();
        if timed_out && buffered < cushion {
            crate::vprintln!("[ASIO] control: started early ({buffered} cushioned, decode slow)");
        } else {
            crate::vprintln!("[ASIO] control: started ({buffered} samples cushioned)");
        }
    }

    fn maybe_time_update(&mut self, event_tx: &mpsc::Sender<AsioEvent>) {
        if !self.client_started || self.last_time_report.elapsed().as_millis() < 200 {
            return;
        }
        let played = self
            .played_frames
            .load(Ordering::Relaxed)
            .saturating_sub(self.played_offset);
        let mut pos = (self.baseline_frames + played) as f64 / self.sample_rate.max(1) as f64;
        if self.duration > 0.0 {
            pos = pos.min(self.duration);
        }
        let _ = event_tx.send(AsioEvent::TimeUpdate(pos));
        self.last_time_report = Instant::now();
    }

    /// Complete the track once EOF is reached, staging is drained, and the ring has
    /// emptied - after a short grace so the last ASIO double-buffer plays out.
    fn maybe_complete(&mut self, event_tx: &mpsc::Sender<AsioEvent>) {
        if !self.stream_ended || !self.staging.is_empty() || !self.client_started {
            return;
        }
        let drained = self
            .producer
            .as_ref()
            .is_none_or(|p| p.slots() >= self.ring_capacity);
        if !drained {
            self.drain_since = None;
            return;
        }
        match self.drain_since {
            Some(since) if since.elapsed() >= Duration::from_millis(120) => {
                // Keep the clock running (continuous hold) -- the RT emits silence until the
                // next track. flush_gen drops the last drained buffer.
                self.flush_gen.fetch_add(1, Ordering::Relaxed);
                let _ = event_tx.send(AsioEvent::TimeUpdate(self.duration));
                let completed_id = self.stream_id.unwrap_or(0);
                let _ = event_tx.send(AsioEvent::Completed(completed_id));
                crate::vprintln3!(
                    "[ASIO-DIAG] maybe_complete: clearing stream={completed_id} (EOF+drained) -> Completed"
                );
                self.stream_id = None;
                self.stream_ended = false;
                self.drain_since = None;
                self.state = AsioState::Idle;
            }
            Some(_) => {}
            None => self.drain_since = Some(Instant::now()),
        }
    }

    /// `ASIOStop` if the clock is running (keeps the buffers for reuse).
    fn stop_driver_if_running(&mut self) {
        if self.client_started {
            if let Some(ref driver) = self.driver {
                // SAFETY: live driver; ASIOStop blocks until the RT thread is quiesced.
                unsafe {
                    driver.stop();
                }
            }
            self.client_started = false;
        }
    }

    /// Stop the clock, dispose the buffers, and reclaim the `StreamCtx`. Order is
    /// load-bearing: ASIOStop first (no callback can still be in flight), then null
    /// `STREAM_CTX`, then free the box - else a live callback would read freed memory.
    fn dispose_stream(&mut self) {
        self.stop_driver_if_running();
        if self.has_buffers
            && let Some(ref driver) = self.driver
        {
            // SAFETY: buffers were created on this driver and the clock is stopped.
            unsafe {
                driver.dispose_buffers();
            }
        }
        STREAM_CTX.store(core::ptr::null_mut(), Ordering::Release);
        if !self.ctx_ptr.is_null() {
            // SAFETY: ASIOStop above guarantees no RT callback is in flight, so the
            // leaked StreamCtx box has no live reader and can be reclaimed.
            unsafe {
                drop(Box::from_raw(self.ctx_ptr));
            }
            self.ctx_ptr = core::ptr::null_mut();
        }
        self.producer = None;
        self.has_buffers = false;
    }

    fn teardown(&mut self) {
        self.dispose_stream();
        // AsioDriver::drop releases the COM reference.
        self.driver = None;
    }

    /// Dispatch one command; returns true when the thread should exit (Shutdown).
    fn dispatch(&mut self, cmd: AsioCommand, event_tx: &mpsc::Sender<AsioEvent>) -> bool {
        match cmd {
            AsioCommand::StartStream {
                stream_id,
                sample_rate,
                channels,
                duration_secs,
                start_secs,
                start_paused,
            } => {
                if self
                    .handle_start_stream(
                        event_tx,
                        stream_id,
                        sample_rate,
                        channels,
                        duration_secs,
                        start_secs,
                        start_paused,
                    )
                    .is_err()
                {
                    // The error event triggers the player's Shared fallback, which
                    // shuts this handle down; idle until then.
                    self.state = AsioState::Idle;
                }
            }
            AsioCommand::PushPcm { stream_id, samples } => self.handle_push_pcm(stream_id, samples),
            AsioCommand::EndStream { stream_id } => self.handle_end_stream(stream_id),
            AsioCommand::ResetForSeek {
                stream_id,
                start_secs,
            } => self.handle_reset_for_seek(stream_id, start_secs),
            AsioCommand::Play => {
                if matches!(self.state, AsioState::Paused) {
                    self.resume(event_tx);
                }
            }
            AsioCommand::Pause => {
                if matches!(self.state, AsioState::Playing) {
                    // Hold the clock (KS-exclusive device kept); the RT emits silence while
                    // `paused` is set instead of ASIOStop-ing (which would free the device).
                    self.paused.store(true, Ordering::Relaxed);
                    self.state = AsioState::Paused;
                    let _ = event_tx.send(AsioEvent::StateChange(PlaybackState::Paused));
                }
            }
            AsioCommand::Stop => self.handle_stop(event_tx),
            AsioCommand::Shutdown => {
                self.teardown();
                return true;
            }
        }
        false
    }

    fn run(&mut self, cmd_rx: mpsc::Receiver<AsioCommand>, event_tx: mpsc::Sender<AsioEvent>) {
        loop {
            match self.state {
                AsioState::Idle => match cmd_rx.recv() {
                    Ok(cmd) => {
                        if self.dispatch(cmd, &event_tx) {
                            return;
                        }
                    }
                    Err(_) => {
                        self.teardown();
                        return;
                    }
                },
                AsioState::Paused => {
                    match cmd_rx.recv() {
                        Ok(cmd) => {
                            if self.dispatch(cmd, &event_tx) {
                                return;
                            }
                        }
                        Err(_) => {
                            self.teardown();
                            return;
                        }
                    }
                    // Keep the ring topped while paused so resume is glitch-free.
                    self.pump_ring();
                    self.update_buffered();
                }
                AsioState::Playing => {
                    // Block briefly for a command, then drain any backlog.
                    match cmd_rx.recv_timeout(Duration::from_millis(4)) {
                        Ok(cmd) => {
                            if self.dispatch(cmd, &event_tx) {
                                return;
                            }
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => {
                            self.teardown();
                            return;
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                    }
                    while let Ok(cmd) = cmd_rx.try_recv() {
                        if self.dispatch(cmd, &event_tx) {
                            return;
                        }
                    }
                    if !matches!(self.state, AsioState::Playing) {
                        continue;
                    }
                    self.pump_ring();
                    self.update_buffered();
                    self.maybe_start(&event_tx);
                    self.maybe_time_update(&event_tx);
                    self.maybe_complete(&event_tx);
                    self.log_status();
                }
            }
        }
    }

    /// Publish the current buffered-sample count (ring + staging) for the decode
    /// thread's back-pressure.
    fn update_buffered(&self) {
        let ring_used = self
            .producer
            .as_ref()
            .map_or(0, |p| self.ring_capacity.saturating_sub(p.slots()));
        self.buffered
            .store((ring_used + self.staging.len()) as u64, Ordering::Relaxed);
    }

    /// Diagnostic: periodically dump ring/staging/playback state so a mid-playback
    /// silence can be classified (ring-starved vs stopped callback vs premature completion).
    fn log_status(&mut self) {
        if self.last_status.elapsed() < Duration::from_secs(1) {
            return;
        }
        self.last_status = Instant::now();
        let ring_used = self
            .producer
            .as_ref()
            .map_or(0, |p| self.ring_capacity.saturating_sub(p.slots()));
        crate::vprintln!(
            "[ASIO] status: ring={}/{} staging={} played={} underruns={} ended={} started={}",
            ring_used,
            self.ring_capacity,
            self.staging.len(),
            self.played_frames.load(Ordering::Relaxed),
            STREAM_UNDERRUNS.load(Ordering::Relaxed),
            self.stream_ended,
            self.client_started,
        );
    }
}

fn control_thread(
    gain: Arc<AtomicU32>,
    buffered: Arc<AtomicU64>,
    cmd_rx: mpsc::Receiver<AsioCommand>,
    event_tx: mpsc::Sender<AsioEvent>,
) {
    ControlCtx::new(gain, buffered).run(cmd_rx, event_tx);
}

/// Player-facing handle to the ASIO control thread. Mirrors `ExclusiveHandle`.
pub(crate) struct AsioHandle {
    cmd_tx: mpsc::Sender<AsioCommand>,
    event_rx: mpsc::Receiver<AsioEvent>,
    /// Shared buffered-sample count (ring + staging) the decode thread reads to
    /// throttle itself; cloned to each decoder via [`AsioHandle::buffered`].
    buffered: Arc<AtomicU64>,
    thread: Option<JoinHandle<()>>,
}

impl AsioHandle {
    /// Spawn the ASIO control thread. It opens the first enumerated ASIO driver on
    /// the first `StartStream`. `gain` is the shared digital-gain cell (f32 bits).
    pub(crate) fn spawn(gain: Arc<AtomicU32>) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel::<AsioCommand>();
        let (event_tx, event_rx) = mpsc::channel::<AsioEvent>();
        let buffered = Arc::new(AtomicU64::new(0));
        let buffered_ctl = buffered.clone();
        let thread = thread::spawn(move || control_thread(gain, buffered_ctl, cmd_rx, event_tx));
        Self {
            cmd_tx,
            event_rx,
            buffered,
            thread: Some(thread),
        }
    }

    pub(crate) fn send(&self, cmd: AsioCommand) {
        let _ = self.cmd_tx.send(cmd);
    }

    pub(crate) fn command_sender(&self) -> mpsc::Sender<AsioCommand> {
        self.cmd_tx.clone()
    }

    /// The shared buffered-sample counter, for the decode thread's back-pressure.
    pub(crate) fn buffered(&self) -> Arc<AtomicU64> {
        self.buffered.clone()
    }

    pub(crate) fn poll_events(&self) -> Vec<AsioEvent> {
        let mut events = Vec::new();
        while let Ok(ev) = self.event_rx.try_recv() {
            events.push(ev);
        }
        events
    }

    pub(crate) fn shutdown(mut self) {
        let _ = self.cmd_tx.send(AsioCommand::Shutdown);
        if let Some(h) = self.thread.take() {
            let _ = h.join();
        }
    }
}

impl Drop for AsioHandle {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(AsioCommand::Shutdown);
        if let Some(h) = self.thread.take() {
            let _ = h.join();
        }
    }
}

/// Decode a `RamBuffer` (the same source the cpal/exclusive paths read) to
/// interleaved i32 and feed the ASIO control thread via `AsioCommand`. Mirrors
/// `wasapi::stream_flac_reader_to_wasapi`, but the ring is i32 so `PushPcm` carries
/// `Vec<i32>` directly (no byte-packing) and the RT host renders it as src_bps=32.
/// `RamBuffer` is itself a `MediaSource`, so no `SizedMediaSource` wrapper is needed.
#[allow(clippy::too_many_arguments)]
pub(crate) fn stream_reader_to_asio(
    reader: RamBuffer,
    stream_id: u32,
    cmd_tx: mpsc::Sender<AsioCommand>,
    cancel: Arc<AtomicBool>,
    seek_to: Option<f64>,
    start_paused: bool,
    seek_rx: mpsc::Receiver<f64>,
    buffered: Arc<AtomicU64>,
) -> Result<(), String> {
    // Cheap read-only handle (shares the Arc<SharedState>): lets the decode loop
    // observe the live reader's cursor/download progress for [ASIO-DIAG] traces.
    let reader_diag = reader.clone();
    let mss = MediaSourceStream::new(Box::new(reader), Default::default());

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
    let n_frames = track.num_frames.unwrap_or(0);
    let duration_secs = if sample_rate > 0 && n_frames > 0 {
        n_frames as f64 / sample_rate as f64
    } else {
        0.0
    };

    // StartStream FIRST at offset 0 so the control thread opens the driver while a
    // forward seek into not-yet-downloaded data resolves; the landing position
    // follows via ResetForSeek (mirrors the exclusive path).
    cmd_tx
        .send(AsioCommand::StartStream {
            stream_id,
            sample_rate,
            channels,
            duration_secs,
            start_secs: 0.0,
            start_paused,
        })
        .map_err(|_| "failed to send StartStream".to_string())?;
    crate::vprintln3!(
        "[ASIO-DIAG] decoder spawned: stream={stream_id} paused={start_paused} seek_to={seek_to:?}"
    );

    let mut pending_initial_seek: Option<f64> = None;
    let mut was_initial_seek = false;
    if let Some(t) = seek_to
        && t > 0.0
    {
        was_initial_seek = true;
        crate::vprintln3!(
            "[ASIO-DIAG] initial seek requested: {t:.1}s start_paused={start_paused} stream={stream_id} | read_cursor={} written={}",
            reader_diag.read_cursor(),
            reader_diag.written(),
        );
        if let Some(time_pos) = symphonia::core::units::Time::try_from_secs_f64(t) {
            match format_reader.seek(
                symphonia::core::formats::SeekMode::Coarse,
                symphonia::core::formats::SeekTo::Time {
                    time: time_pos,
                    track_id: Some(track.id),
                },
            ) {
                Ok(seeked) => {
                    let actual = if sample_rate > 0 {
                        seeked.actual_ts.get() as f64 / sample_rate as f64
                    } else {
                        t
                    };
                    crate::vprintln!("[ASIO] decoder seek to {t:.1}s (actual {actual:.1}s)");
                    crate::vprintln3!(
                        "[ASIO-DIAG] post-seek read_cursor={} written={} stalled={}",
                        reader_diag.read_cursor(),
                        reader_diag.written(),
                        reader_diag.is_stalled(),
                    );
                    if cmd_tx
                        .send(AsioCommand::ResetForSeek {
                            stream_id,
                            start_secs: actual,
                        })
                        .is_err()
                    {
                        return Ok(());
                    }
                }
                Err(e) => {
                    crate::vprintln!("[ASIO] decoder seek to {t:.1}s failed, will retry: {e}");
                    pending_initial_seek = Some(t);
                }
            }
        }
    }

    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(codec_params, &AudioDecoderOptions::default())
        .map_err(|e| format!("decoder creation failed: {e}"))?;

    // Back-pressure target (~2 s of samples, matching the ring): decoding further ahead
    // races the streaming download window and stalls the read.
    let throttle_hi = sample_rate as u64 * channels as u64 * 2;

    loop {
        if cancel.load(Ordering::Relaxed) {
            return Ok(());
        }

        // Throttle to ~real time: if the buffer already holds the target, wait rather
        // than decode further ahead of the download.
        if buffered.load(Ordering::Relaxed) > throttle_hi {
            let throttle_t0 = Instant::now();
            crate::vprintln3!(
                "[ASIO-DIAG] throttle wait: buffered={} > hi={throttle_hi}",
                buffered.load(Ordering::Relaxed),
            );
            while !cancel.load(Ordering::Relaxed) && buffered.load(Ordering::Relaxed) > throttle_hi
            {
                // A seek can arrive while paused-and-full (the RT holds the ring, so `buffered`
                // stays above the throttle); break to the seek handler below instead of spinning
                // until resume, which would play stale pre-seek audio.
                if let Ok(t) = seek_rx.try_recv() {
                    pending_initial_seek = Some(t);
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            crate::vprintln3!(
                "[ASIO-DIAG] throttle resume after {:.0}ms buffered={}",
                throttle_t0.elapsed().as_secs_f64() * 1000.0,
                buffered.load(Ordering::Relaxed),
            );
        }
        if cancel.load(Ordering::Relaxed) {
            return Ok(());
        }

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
                    crate::vprintln!("[ASIO] live seek to {t:.1}s (actual {actual:.1}s)");
                    if cmd_tx
                        .send(AsioCommand::ResetForSeek {
                            stream_id,
                            start_secs: actual,
                        })
                        .is_err()
                    {
                        return Ok(());
                    }
                }
                Err(e) => {
                    crate::vprintln!("[ASIO] live seek to {t:.1}s failed: {e}");
                    if was_initial_seek {
                        pending_initial_seek = Some(t);
                    }
                }
            }
        }

        let pkt_t0 = Instant::now();
        let packet = match format_reader.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) => break,
            Err(e) => {
                if cancel.load(Ordering::Relaxed) {
                    return Ok(());
                }
                return Err(format!("decode packet error: {e}"));
            }
        };
        // A long next_packet() means the decoder blocked in RamBuffer::read (starvation);
        // log buffer geometry to correlate with the [BUFFER] read-miss storm.
        let pkt_ms = pkt_t0.elapsed().as_secs_f64() * 1000.0;
        if pkt_ms > 200.0 {
            crate::vprintln3!(
                "[ASIO-DIAG] next_packet blocked {pkt_ms:.0}ms | read_cursor={} written={} stalled={} buffered={}",
                reader_diag.read_cursor(),
                reader_diag.written(),
                reader_diag.is_stalled(),
                buffered.load(Ordering::Relaxed),
            );
        }

        if packet.track_id != track.id {
            continue;
        }

        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(_) => continue,
        };

        let mut samples: Vec<i32> = Vec::new();
        decoded.copy_to_vec_interleaved::<i32>(&mut samples);

        if !samples.is_empty()
            && cmd_tx
                .send(AsioCommand::PushPcm { stream_id, samples })
                .is_err()
        {
            return Ok(());
        }
    }

    if !cancel.load(Ordering::Relaxed) {
        crate::vprintln!("[ASIO] decode: EOF (stream {stream_id}), sent EndStream");
        let _ = cmd_tx.send(AsioCommand::EndStream { stream_id });
    } else {
        crate::vprintln!("[ASIO] decode: cancelled (stream {stream_id})");
    }
    Ok(())
}
