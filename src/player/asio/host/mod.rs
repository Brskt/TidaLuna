//! ASIO real-time host (Windows only).
//!
//! The driver invokes `bufferSwitch` on its own real-time thread. The four ASIO
//! callbacks are bare C function pointers with no `this` and recover state
//! from a process-global pointer; ASIO loads one driver at a time, making a single
//! global fits. The callback must never allocate, lock, block, or panic (a panic
//! crossing `extern "system"` aborts the process).

use core::cell::UnsafeCell;
use core::ffi::c_void;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use super::convert::{AsioSampleType, apply_gain, write_dst_sample};
use super::iasio::{AsioBool, AsioSampleRate, AsioTime, output_ready_raw};
use crate::player::PlaybackState;
use crate::player::declick::{fade_in_env, fade_out_env, fade_scale};
use crate::player::packet_failure::PacketFailure;

mod control;
mod feeder;
mod smoke;

pub(crate) use control::AsioHandle;
pub(crate) use feeder::stream_reader_to_asio;
use smoke::tone_buffer_switch;
pub(crate) use smoke::{run_flac_test, run_ring_tone_test, run_tone_test};

/// Stereo: TIDAL streams only stereo PCM, and ASIO4ALL exposes 2 output channels.
const CHANNELS: usize = 2;

/// `sampleRateDidChange`: the driver's clock changed; logged only.
unsafe extern "system" fn tone_sample_rate_did_change(s_rate: AsioSampleRate) {
    crate::vprintln!("[ASIO] sampleRateDidChange: {s_rate} Hz");
}

/// `asioMessage`: the driver queries host capabilities and posts requests. We must
/// acknowledge `kAsioResetRequest`/`kAsioResyncRequest` (a stub returning 0 leaves the
/// clock never started on an endpoint renegotiation, e.g. ASIO4ALL's KS pin for a high
/// rate); deferred to the control thread via `STREAM_RESET_REQUESTED` since re-entering
/// ASIO from the driver thread is forbidden. `kAsioSupportsTimeInfo` stays unadvertised.
unsafe extern "system" fn tone_asio_message(
    selector: i32,
    value: i32,
    _message: *mut c_void,
    _opt: *mut f64,
) -> i32 {
    const KASIO_SELECTOR_SUPPORTED: i32 = 1;
    const KASIO_ENGINE_VERSION: i32 = 2;
    const KASIO_RESET_REQUEST: i32 = 3;
    const KASIO_BUFFER_SIZE_CHANGE: i32 = 4;
    const KASIO_RESYNC_REQUEST: i32 = 5;
    const KASIO_LATENCIES_CHANGED: i32 = 6;
    match selector {
        KASIO_SELECTOR_SUPPORTED => match value {
            KASIO_RESET_REQUEST
            | KASIO_BUFFER_SIZE_CHANGE
            | KASIO_RESYNC_REQUEST
            | KASIO_LATENCIES_CHANGED => 1,
            _ => 0,
        },
        KASIO_ENGINE_VERSION => 2,
        // Defer to the control thread; it polls the flag and routes this track to shared.
        KASIO_RESET_REQUEST | KASIO_BUFFER_SIZE_CHANGE => {
            STREAM_RESET_REQUESTED.store(true, Ordering::Relaxed);
            1
        }
        // Acknowledged; no teardown needed (the resync silence already masks any relock).
        KASIO_RESYNC_REQUEST | KASIO_LATENCIES_CHANGED => 1,
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

// Streaming path: decoded PCM -> lock-free ring -> ASIO RT callback

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
    /// Digital gain as f32 bits (1.0 = bit-perfect passthrough). Live, letting the
    /// control thread drive it from the volume slider.
    gain: Arc<AtomicU32>,
    /// Lock-free ring consumer; touched only on the driver RT thread.
    consumer: UnsafeCell<rtrb::Consumer<i32>>,
    /// Pre-allocated interleaved-i32 scratch (`frames * channels`).
    scratch: UnsafeCell<Box<[i32]>>,
    /// Bumped by the control thread on seek/track-change; the RT callback drains
    /// the ring once when it changes, keeping stale pre-seek audio from being played
    /// (mirrors the cpal `seek_gen` drain in `thread/output/mixer.rs`).
    flush_gen: Arc<AtomicU32>,
    /// The RT thread's last-seen flush generation (touched only on the RT thread).
    local_flush_gen: AtomicU32,
    /// Real frames the RT callback has consumed from the ring (excludes underrun
    /// zero-fill); the control thread reads it for position reporting.
    played_frames: Arc<AtomicU64>,
    /// Set on pause: the RT callback emits silence without consuming the ring or
    /// advancing position, leaving the driver clock running (device held).
    paused: Arc<AtomicBool>,
    /// De-click envelope length in frames (DECLICK_FADE_MS at this stream's rate); shared by the
    /// teardown fade-out and the post-resync fade-in.
    fade_len: usize,
    /// Control thread sets this to request a fade-to-silence before a format-change
    /// teardown; the RT ramps the output down then holds zero (avoids the pre-stop click).
    fade_out: AtomicBool,
    /// RT sets this once a fully-silent buffer has reached the *playing* half (one switch
    /// after the ramp), letting the control thread `ASIOStop` without cutting a non-zero sample.
    fade_out_done: AtomicBool,
    /// RT-only: fully-silent buffers emitted since the ramp finished. `fade_out_done` is set
    /// on the 2nd, when the 1st is guaranteed to be the audible half.
    fade_out_silence: AtomicUsize,
    /// RT-only: frames elapsed in the teardown fade-out.
    fade_out_pos: AtomicUsize,
    /// Post-start resync silence remaining (frames): the RT emits zeros without consuming
    /// the ring, making the DAC PLL relock at the new rate inaudible.
    intro_silence: AtomicUsize,
    /// Frames of fade-in remaining after the resync silence (0 -> unity over `fade_len`).
    intro_fade: AtomicUsize,
}

static STREAM_CTX: AtomicPtr<StreamCtx> = AtomicPtr::new(core::ptr::null_mut());

/// Diagnostic: RT-callback underruns (ring drained, tail zero-filled).
static STREAM_UNDERRUNS: AtomicU64 = AtomicU64::new(0);

/// Diagnostic: total `stream_buffer_switch` invocations. If this stays flat while the control
/// thread reports `started=true`, the driver accepted the rate but isn't clocking it.
static STREAM_SWITCHES: AtomicU64 = AtomicU64::new(0);

/// Set by `asio_message` on a driver `kAsioResetRequest`/`kAsioBufferSizeChange`; the control
/// thread polls it and rebuilds the stream in place (capped; a reset loop gives up to
/// `RateUnsupported`: per-track shared, ASIO stays on). See `handle_reset_request`.
static STREAM_RESET_REQUESTED: AtomicBool = AtomicBool::new(false);

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
    STREAM_SWITCHES.fetch_add(1, Ordering::Relaxed);
    // SAFETY: `STREAM_CTX` is a leaked `Box<StreamCtx>` set before `ASIOStart` and
    // never freed, leaving the pointee live for any in-flight callback.
    let ctx = unsafe { &*ctx };
    let half = (double_buffer_index & 1) as usize;
    let frames = ctx.frames;
    let channels = ctx.channels;
    let n = frames * channels;

    // SAFETY: `scratch` and `consumer` are touched only here, on the single driver
    // RT thread, between `start` and `stop`; no other live reference exists.
    let scratch = unsafe { &mut *ctx.scratch.get() };
    let consumer = unsafe { &mut *ctx.consumer.get() };

    // Flush-gen changed (seek / track-change): drain the ring once; stale
    // pre-seek audio is not played, then adopt the new generation. Mirrors the
    // cpal `seek_gen` drain in `thread/output/mixer.rs`.
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

    // De-click for the rate-change pop. Priority:
    //   fade_out (teardown ramp) > paused > intro_silence (resync) > normal (+ fade-in).
    if ctx.fade_out.load(Ordering::Relaxed) {
        // Format-change teardown requested: ramp the live output down to zero then hold
        // silence, and the imminent ASIOStop never cuts a non-zero sample (pre-stop click).
        let len = ctx.fade_len.max(1);
        let pos = ctx.fade_out_pos.load(Ordering::Relaxed);
        if pos < len {
            let to_read = consumer.slots().min(n);
            if to_read > 0
                && let Ok(chunk) = consumer.read_chunk(to_read)
            {
                let (s1, s2) = chunk.as_slices();
                scratch[..s1.len()].copy_from_slice(s1);
                scratch[s1.len()..to_read].copy_from_slice(s2);
                chunk.commit_all();
            }
            for s in &mut scratch[to_read..n] {
                *s = 0;
            }
            for f in 0..frames {
                let p = (pos + f).min(len);
                let env = fade_out_env(p, len);
                for ch in 0..channels {
                    let i = f * channels + ch;
                    scratch[i] = fade_scale(scratch[i], env);
                }
            }
            ctx.fade_out_pos
                .store((pos + frames).min(len), Ordering::Relaxed);
        } else {
            for s in &mut scratch[..n] {
                *s = 0;
            }
            // ASIO plays one buffer ahead: signal done on the 2nd silent fill, when the 1st is
            // the playing half, and ASIOStop truncates silence, not the last (non-zero) ramp.
            if ctx.fade_out_silence.fetch_add(1, Ordering::Relaxed) >= 1 {
                ctx.fade_out_done.store(true, Ordering::Relaxed);
            }
        }
    } else if ctx.paused.load(Ordering::Relaxed) {
        // Paused: emit silence without consuming the ring (preserve it for instant
        // resume) or advancing position. The flush-gen drain above still ran.
        for s in &mut scratch[..n] {
            *s = 0;
        }
    } else if ctx.intro_silence.load(Ordering::Relaxed) > 0 {
        // Post-start resync silence: hold zero while the DAC PLL relocks at the new rate.
        // Do NOT consume the ring or advance position. The track head is delayed, not
        // dropped; it plays out after the silence with a fade-in.
        for s in &mut scratch[..n] {
            *s = 0;
        }
        let rem = ctx.intro_silence.load(Ordering::Relaxed);
        ctx.intro_silence
            .store(rem.saturating_sub(frames), Ordering::Relaxed);
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
        // Fade in the new track's head after the resync silence.
        let fin = ctx.intro_fade.load(Ordering::Relaxed);
        if fin > 0 {
            let len = ctx.fade_len.max(1);
            let pos = len - fin.min(len);
            for f in 0..frames {
                let p = (pos + f).min(len);
                let env = fade_in_env(p, len);
                for ch in 0..channels {
                    let i = f * channels + ch;
                    scratch[i] = fade_scale(scratch[i], env);
                }
            }
            ctx.intro_fade
                .store(fin.saturating_sub(frames), Ordering::Relaxed);
        }
        // Count only the real frames consumed (not the underrun zero-fill), keeping the
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

// The player-integrated AsioHandle.
//
// Mirrors `wasapi::ExclusiveHandle`. A control thread owns the `AsioDriver` and
// the `rtrb::Producer<i32>`; the driver's RT `stream_buffer_switch` owns the
// `Consumer` via `STREAM_CTX`. Decode threads feed audio as `AsioCommand::PushPcm`
// (a cloneable mpsc) because rtrb is SPSC - the control thread is the sole writer.

/// Commands into the ASIO control thread. `PushPcm` carries interleaved i32
/// (MSB-justified, from `copy_to_vec_interleaved::<i32>`) - the ring is i32, so
/// no byte-packing step (unlike the WASAPI `PushPcm`).
pub(crate) enum AsioCommand {
    StartStream {
        stream_id: u32,
        /// The seq of the load whose decoder opened the stream, echoed on every transport state
        /// the control thread reports for it. Meaningful to the player alone, as `gen_id` is to
        /// a seek.
        opened_under: u32,
        sample_rate: u32,
        channels: u32,
        duration_secs: f64,
        start_secs: f64,
        start_paused: bool,
        consumed: Arc<AtomicU64>,
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
        gen_id: u32,
        start_secs: f64,
    },
    /// The decoder could not seek. Carries no position: the control thread owns the answer
    /// and reports the seek unsettled, leaving the player to fall back on what it last knew.
    SeekFailed {
        stream_id: u32,
        gen_id: u32,
    },
    /// The decoder thread died mid-stream. It is the one exit that signals nothing on its
    /// own, which left the player holding a seek channel whose receiver was already gone.
    /// `cause` travels with it because the classification is only available here, where the
    /// error is still typed: the text alone cannot be re-read for it further down.
    DecodeFailed {
        stream_id: u32,
        error: String,
        cause: PacketFailure,
    },
    /// The decode thread died BEFORE adopting the stream. `DecodeFailed` cannot carry
    /// it: that variant only ever travels after this stream's own `StartStream`. Without
    /// this the failure reached nobody (the spawn's caller only logs, behind a gate that
    /// is off by default), leaving the transport at Ready with nothing playing and no skip.
    StartupFailed {
        stream_id: u32,
        error: String,
        cause: PacketFailure,
    },
    Play {
        stream_id: u32,
    },
    Pause {
        stream_id: u32,
    },
    Shutdown,
}

/// Events out of the ASIO control thread. `DriverNotFound`/`FormatUnsupported`/
/// `InitFailed` map to the matching `DeviceErrorKind::Asio*` + a Shared fallback.
pub(crate) enum AsioEvent {
    TimeUpdate(f64),
    /// The answer to one dispatched seek, sent whatever the outcome. `position` is where
    /// playback actually is (the landing point when the decoder seeked, the untouched
    /// current position when it refused), never the target nobody reached. `refused`
    /// drives the resume store: a refused target must be evicted, not persisted. Distinct
    /// from `TimeUpdate`, sparing the player any guess about whether a periodic report is
    /// this seek's answer; `stream_id`-scoped like `Completed`, keeping an ack off the
    /// track that replaced it.
    SeekSettled {
        stream_id: u32,
        /// Echoed back from the command untouched. Judging an ack's freshness belongs to
        /// the player thread alone, the only party that knows which seek it awaits.
        gen_id: u32,
        position: f64,
        refused: bool,
    },
    /// A transport state of a stream, named by the load that opened it, which the player relays
    /// whichever stream that is: a pause is reported after the clock it stops, which can
    /// outlast the track change that retired the stream.
    StateChange {
        opened_under: u32,
        state: PlaybackState,
    },
    /// The adopted stream's track length. Stream-scoped like `Completed`: a superseded
    /// stream reports one too, and downstream nothing can tell whose it was.
    Duration {
        stream_id: u32,
        secs: f64,
    },
    DriverNotFound,
    /// The driver cannot render this stream: it exposes fewer output channels than the track
    /// asks for, or a sample type this host does not convert. Stream-scoped like
    /// `RateUnsupported`, the channel count being the TRACK's and not the device's alone.
    FormatUnsupported {
        stream_id: Option<u32>,
    },
    /// The device can't play this track's format in ASIO: either the rate was rejected up front
    /// (`can_sample_rate`/`set_sample_rate`) or the driver posted a reset it needs to clock it.
    /// Per-track: route THIS track to shared but keep ASIO on for other rates. Stream-scoped
    /// like `Completed`; `None` when the build ran before any stream was adopted, and a
    /// verdict that cannot name its stream marks no track.
    RateUnsupported {
        stream_id: Option<u32>,
    },
    InitFailed(String),
    /// Track finished (EOF + drained). Carries the stream_id: a stale completion from a
    /// superseded stream can't clear a newer track (which would force a re-arm/double-load).
    Completed(u32),
    /// The decoder thread died mid-stream. Settled like the shared path's fatal decode
    /// error, never like `Completed`: the track did not finish, and its resume point has
    /// to survive. Stream-scoped, keeping a superseded decoder's death off a newer track.
    DecodeFailed {
        stream_id: u32,
        error: String,
        cause: PacketFailure,
    },
    /// The decoder thread died before adopting the stream. Settled exactly like `DecodeFailed`,
    /// and it carries the same cause for the same reason: the probe reads the streaming buffer,
    /// which is the network path; a stall at track start raises `TimedOut` here just as it
    /// does mid-track. Classified where the typed error still exists, since the `format!` below
    /// leaves nothing to classify.
    StartupFailed {
        stream_id: u32,
        error: String,
        cause: PacketFailure,
    },
}
