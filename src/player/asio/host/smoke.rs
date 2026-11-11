use core::cell::UnsafeCell;
use core::sync::atomic::{
    AtomicBool, AtomicI32, AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering,
};
use std::fs::File;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use symphonia::core::codecs::CodecParameters;
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::formats::FormatOptions;
use symphonia::core::formats::probe::Hint;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use windows_sys::Win32::UI::WindowsAndMessaging::GetDesktopWindow;

use super::{
    CHANNELS, STREAM_CTX, StreamCtx, asio_message, sample_rate_did_change, stream_buffer_switch,
    stream_buffer_switch_time_info,
};
use crate::player::asio::convert::{AsioSampleType, write_dst_sample};
use crate::player::asio::driver::AsioDriverInfo;
use crate::player::asio::iasio::{
    AsioBool, AsioBufferInfo, AsioCallbacks, AsioChannelInfo, AsioDriver, AsioTime, asio_ok,
    output_ready_raw,
};

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
    /// Raw `IASIO` COM pointer, letting the callback signal `outputReady`.
    com: *mut core::ffi::c_void,
    /// Absolute frame index of the next buffer, keeping the tone phase-continuous.
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
    const AMP: f64 = 0.2 * i32::MAX as f64; // A fifth of full scale, headphone-safe
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
    // is never freed, leaving the pointee live for any in-flight callback.
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
        // stay within `frames * bps` and `frames * CHANNELS`. Neither panics.
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

/// `bufferSwitchTimeInfo` for the tone: unadvertised like the streaming one, and forwarding
/// to the tone's own switch.
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
    // keeping the pointers the driver retains valid for the whole session.
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
        // its capabilities, letting a device/buffer change apply before `createBuffers`.
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
            sample_rate_did_change,
            asio_message,
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
            "[ASIO] tone: done - bufferSwitch fired {switches} times (expected {expected})"
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
            sample_rate_did_change,
            asio_message,
            buffer_switch_time_info: stream_buffer_switch_time_info,
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

        // Pre-fill a cushion, keeping the first callbacks from underrunning.
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
            // De-click is inert on the smoke-test path (it never changes rate); the real
            // resync/fade values are armed in build_stream + handle_start_stream.
            fade_len: 0,
            fade_out: AtomicBool::new(false),
            fade_out_done: AtomicBool::new(false),
            fade_out_silence: AtomicUsize::new(0),
            fade_out_pos: AtomicUsize::new(0),
            intro_silence: AtomicUsize::new(0),
            intro_fade: AtomicUsize::new(0),
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
    const MAX_RING: usize = 192_000 * 2 * 2; // two seconds at 192 kHz stereo
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
    const AMP: f64 = 0.2 * i32::MAX as f64; // A fifth of full scale, headphone-safe
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
    const MAX_RING: usize = 48_000 * 2 * 2; // two seconds at 48 kHz stereo
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
