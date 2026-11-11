use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use windows_sys::Win32::UI::WindowsAndMessaging::GetDesktopWindow;

use super::{
    AsioCommand, AsioEvent, CHANNELS, STREAM_CTX, STREAM_RESET_REQUESTED, STREAM_SWITCHES,
    STREAM_UNDERRUNS, StreamCtx, stream_buffer_switch, tone_asio_message,
    tone_buffer_switch_time_info, tone_sample_rate_did_change,
};
use crate::player::PlaybackState;
use crate::player::asio::convert::AsioSampleType;
use crate::player::asio::driver::{
    AsioDriverInfo, driver_error_message, enumerate_asio_drivers, open_candidates,
};
use crate::player::asio::iasio::{
    AsioBufferInfo, AsioCallbacks, AsioChannelInfo, AsioDriver, asio_ok,
};
use crate::player::declick::{
    DECLICK_FADE_MS, RESYNC_SILENCE_MS, fade_out_wait_ms, silence_frames,
};
use crate::player::packet_failure::PacketFailure;

/// Absolute frame index for a position in seconds, the `played_frames` baseline so
/// reporting stays correct after a seek/resume. Mirrors `wasapi::render::position_frames`.
fn position_frames(secs: f64, sample_rate: u32) -> u64 {
    if sample_rate > 0 && secs.is_finite() && secs > 0.0 {
        (secs * sample_rate as f64) as u64
    } else {
        0
    }
}

#[derive(Clone, Copy)]
enum AsioState {
    Idle,
    Playing,
    Paused,
    /// A format change is pending over a live clock: the RT is fading the old stream
    /// to silence and the run loop polls `fade_out_done` between commands, running
    /// `finish_rebuild` when it lands or `deadline` elapses. A dedicated state keeps
    /// the loop on a bounded recv: a pending rebuild can never starve behind the
    /// Idle/Paused arms' blocking `recv()`.
    Rebuilding {
        deadline: Instant,
        start_paused: bool,
    },
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
/// only on that thread), and needs no `Send`.
struct ControlCtx {
    /// Shared digital gain (f32 bits), cloned into each `StreamCtx`.
    gain: Arc<AtomicU32>,
    /// The adopted stream's consumed counter (minted per decoder spawn):
    /// credited as samples leave staging or are discarded, keeping the decoder's
    /// sent-minus-consumed throttle honest even while this thread is stuck
    /// opening the driver.
    consumed: Arc<AtomicU64>,
    driver: Option<AsioDriver>,
    /// The driver name the device switch asked for, when it named one at all.
    /// `None` means "whichever installed driver opens first".
    requested_driver: Option<String>,
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
    /// consuming the ring, leaving the driver clock held (device kept) during a pause.
    paused: Arc<AtomicBool>,
    /// Heap-stable callbacks struct handed to `createBuffers` (kept alive for the
    /// driver's lifetime).
    callbacks: Box<AsioCallbacks>,
    stream_id: Option<u32>,
    /// The load that opened the adopted stream, echoed on its transport states. Written with
    /// `stream_id`, meaningless without one.
    opened_under: u32,
    /// Pre-adoption transport intent `(stream_id, play)`: a Play/Pause for a
    /// stream whose probe-delayed StartStream hasn't arrived yet (FIFO makes
    /// this deterministic). Consumed only by the adoption of the matching id; an
    /// intent for a different, not-yet-adopted stream is left in place (a deferred
    /// rebuild's slow settle must not drain a later skip's intent).
    pending_transport: Option<(u32, bool)>,
    sample_rate: u32,
    channels: usize,
    frames: usize,
    duration: f64,
    /// Frame index of the stream start (from `start_secs`).
    baseline_frames: u64,
    /// `played_frames` snapshot at the last start/seek, subtracted, leaving position
    /// relative to this stream rather than the lifetime of the `played_frames` cell.
    played_offset: u64,
    stream_ended: bool,
    pending_start: bool,
    /// When the deferred (cold) start began waiting for a cushion; bounds the wait
    /// keeping a slow/stalled decode from leaving ASIO silent forever.
    pending_since: Option<Instant>,
    client_started: bool,
    has_buffers: bool,
    /// Set when the ring first drains after EOF; completion fires after a short
    /// grace, letting the last ASIO double-buffer play out.
    drain_since: Option<Instant>,
    last_time_report: Instant,
    /// Throttle for the diagnostic status log.
    last_status: Instant,
    /// Driver-requested resets handled for the current stream; bounds a pathological reset loop
    /// (reset to 0 on each fresh `StartStream`).
    reset_count: u32,
    /// The leaked-then-reclaimed `StreamCtx` pointer (null when no stream is open).
    ctx_ptr: *mut StreamCtx,
    state: AsioState,
}

impl ControlCtx {
    fn new(gain: Arc<AtomicU32>, requested_driver: Option<String>) -> Self {
        Self {
            gain,
            consumed: Arc::new(AtomicU64::new(0)),
            driver: None,
            requested_driver,
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
            opened_under: 0,
            pending_transport: None,
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
            reset_count: 0,
            ctx_ptr: core::ptr::null_mut(),
            state: AsioState::Idle,
        }
    }

    /// Open ASIO buffers on the current driver, install the RT callback + a fresh
    /// `StreamCtx`, and return the producer side of its ring. The driver must
    /// already be created and `init`'d.
    fn build_stream(
        &self,
        stream_id: Option<u32>,
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
                return Err(AsioEvent::RateUnsupported { stream_id });
            }
            // Some clock-locked interfaces accept can_sample_rate but reject the switch;
            // rendering at the old clock plays wrong-speed. Fall back to shared.
            if !asio_ok(driver.set_sample_rate(sample_rate as f64)) {
                return Err(AsioEvent::RateUnsupported { stream_id });
            }
            // set_sample_rate can return ASE_OK yet leave the driver on its old clock
            // (ASIO4ALL/Steinberg-style drivers apply a rate change only after a full
            // reload). createBuffers + start would then run at a phantom rate and
            // bufferSwitch never fires: the "clock won't start after a rate change"
            // hang. Read the rate back and require a match; poll briefly, since a few
            // drivers need a moment to report the new value (JUCE sleeps after setSampleRate).
            let target = sample_rate as f64;
            let mut actual = 0.0f64;
            let mut applied = false;
            for _ in 0..5 {
                if asio_ok(driver.get_sample_rate(&mut actual)) && (actual - target).abs() < 1.0 {
                    applied = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            if !applied {
                crate::vprintln!(
                    "[ASIO] set_sample_rate({target} Hz) not applied (driver reports {actual} Hz); this track plays shared"
                );
                return Err(AsioEvent::RateUnsupported { stream_id });
            }

            let (mut num_in, mut num_out) = (0i32, 0i32);
            if !asio_ok(driver.get_channels(&mut num_in, &mut num_out)) {
                return Err(AsioEvent::InitFailed("getChannels failed".into()));
            }
            if num_out < channels as i32 {
                return Err(AsioEvent::FormatUnsupported { stream_id });
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
                return Err(AsioEvent::FormatUnsupported { stream_id });
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

            // Two-second ring, at least 8 ASIO buffers, enough for the pre-fill cushion to fit.
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
                fade_len: silence_frames(sample_rate, DECLICK_FADE_MS),
                fade_out: AtomicBool::new(false),
                fade_out_done: AtomicBool::new(false),
                fade_out_silence: AtomicUsize::new(0),
                fade_out_pos: AtomicUsize::new(0),
                intro_silence: AtomicUsize::new(0),
                intro_fade: AtomicUsize::new(0),
            };
            crate::vprintln!(
                "[ASIO] stream opened: {sample_rate} Hz / {channels}ch / {frames} frames / type {} / readback {actual:.0} Hz / buffers {min}-{max} (pref {pref}, gran {gran})",
                ch_info.sample_type
            );
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

    /// Arm the RT fade-to-silence, adopt the new stream's identity (so intervening
    /// commands target it), and hand the device rebuild to the run-loop `Rebuilding`
    /// poll, not blocking the control thread for the whole fade window.
    #[expect(clippy::too_many_arguments)]
    fn begin_deferred_rebuild(
        &mut self,
        event_tx: &mpsc::Sender<AsioEvent>,
        stream_id: u32,
        opened_under: u32,
        sample_rate: u32,
        want_channels: usize,
        duration_secs: f64,
        start_secs: f64,
        start_paused: bool,
        consumed: Arc<AtomicU64>,
    ) {
        // SAFETY: the caller checked ctx_ptr is non-null; the leaked StreamCtx stays
        // live until finish_rebuild's dispose_stream, and the RT only reads these atomics.
        let ctx = unsafe { &*self.ctx_ptr };
        ctx.fade_out.store(true, Ordering::Relaxed);
        // Deadline from the OLD (fading) stream's buffer period, computed before the
        // adoption below overwrites the format fields with the new stream's.
        let wait_ms = fade_out_wait_ms(self.frames, self.sample_rate, ctx.fade_len);
        let deadline = Instant::now() + Duration::from_millis(wait_ms);
        crate::vprintln!(
            "[ASIO] deferred rebuild armed: {}Hz -> {sample_rate}Hz (fade wait <= {wait_ms}ms, start_paused={start_paused})",
            self.sample_rate
        );
        self.adopt_stream_identity(
            event_tx,
            stream_id,
            opened_under,
            sample_rate,
            want_channels,
            duration_secs,
            start_secs,
            consumed,
        );
        self.state = AsioState::Rebuilding {
            deadline,
            start_paused,
        };
    }

    /// Finish a deferred format-change rebuild: tear the faded stream down and open the
    /// new one. The logical identity was adopted at arm time; only the device resources
    /// and the transport settle here. ASIOStop/createBuffers stay synchronous
    /// driver-paced COM on this thread (STA affinity); the deferral removes the fade
    /// wait, not the driver's own stop/rebuild cost.
    fn finish_rebuild(&mut self, start_paused: bool, event_tx: &mpsc::Sender<AsioEvent>) {
        self.dispose_stream();
        // Drop a driver reset aimed at the stream just disposed; one posted for the new
        // stream during/after build_stream below is preserved.
        STREAM_RESET_REQUESTED.store(false, Ordering::Relaxed);
        match self.build_stream(self.stream_id, self.sample_rate, self.channels) {
            Ok(opened) => {
                self.install_stream(opened, true, self.sample_rate);
                self.settle_transport(start_paused, event_tx);
            }
            Err(ev) => {
                // The error event triggers the player's Shared fallback, which
                // shuts this handle down; idle until then.
                let _ = event_tx.send(ev);
                self.state = AsioState::Idle;
            }
        }
    }

    #[expect(clippy::too_many_arguments)]
    fn handle_start_stream(
        &mut self,
        event_tx: &mpsc::Sender<AsioEvent>,
        stream_id: u32,
        opened_under: u32,
        sample_rate: u32,
        channels: u32,
        duration_secs: f64,
        start_secs: f64,
        start_paused: bool,
        consumed: Arc<AtomicU64>,
    ) -> Result<(), ()> {
        let want_channels = (channels as usize).clamp(1, CHANNELS);

        // First stream: open the driver the switch named, else the first enumerated
        // one that opens. It persists across same-format tracks.
        if self.driver.is_none() {
            let installed = enumerate_asio_drivers();
            let candidates = open_candidates(&installed, self.requested_driver.as_deref());
            if candidates.is_empty() {
                let _ = event_tx.send(AsioEvent::DriverNotFound);
                return Err(());
            }
            let mut last_error = String::new();
            for info in candidates {
                match try_open_driver(info) {
                    Ok(driver) => {
                        self.driver = Some(driver);
                        self.has_buffers = false;
                        break;
                    }
                    Err(reason) => {
                        last_error = format!("'{}' {reason}", info.name);
                        crate::vprintln!("[ASIO] control: {last_error}");
                    }
                }
            }
            if self.driver.is_none() {
                let _ = event_tx.send(AsioEvent::InitFailed(last_error));
                return Err(());
            }
        }

        let format_changed =
            self.has_buffers && (self.sample_rate != sample_rate || self.channels != want_channels);

        // Live clock: defer teardown to the Rebuilding poll; the fade doesn't stall queued
        // commands/position; a stopped/paused clock is already silent, and the sync path is fine.
        if format_changed
            && self.client_started
            && !self.ctx_ptr.is_null()
            && !self.paused.load(Ordering::Relaxed)
        {
            self.begin_deferred_rebuild(
                event_tx,
                stream_id,
                opened_under,
                sample_rate,
                want_channels,
                duration_secs,
                start_secs,
                start_paused,
                consumed,
            );
            return Ok(());
        }

        if !self.has_buffers || format_changed {
            // Rate change tears the stream down and re-creates buffers at the new rate (RME
            // changes buffer size + channel count per rate, ruling reuse out).
            self.dispose_stream();
            match self.build_stream(Some(stream_id), sample_rate, want_channels) {
                Ok(opened) => self.install_stream(opened, format_changed, sample_rate),
                Err(ev) => {
                    let _ = event_tx.send(ev);
                    return Err(());
                }
            }
        } else {
            // Adopting a DIFFERENT stream over a clock that never started: the ring still
            // holds the superseded stream's PCM, which would otherwise PLAY (a cold-restored
            // armed track bleeding into the next select) since maybe_start assumes a fresh
            // ring. The clock is stopped here, leaving the control thread free to drain it.
            if !self.client_started && self.stream_id != Some(stream_id) {
                self.discard_ring_leftovers();
            }
            // Reuse the open buffers; the clock stays RUNNING (continuous device hold).
            // Bump flush_gen, making the RT callback drain the previous track's audio from the
            // ring on its next tick. The control thread must NOT touch the consumer here (the
            // RT thread owns it while the clock runs); pump_ring is gated on the RT having
            // drained (local_flush_gen == flush_gen), keeping the new track's head.
            self.flush_gen.fetch_add(1, Ordering::Relaxed);
            // Same-format reuse keeps the clock; clear any unfinished resync silence / fade-in
            // left from a just-prior rate change, keeping it from clipping the head of this same-rate track.
            if !self.ctx_ptr.is_null() {
                // SAFETY: ctx_ptr is the live leaked StreamCtx; Relaxed stores of these
                // standalone counters race-free with the RT's Relaxed reads.
                unsafe {
                    let c = &*self.ctx_ptr;
                    c.intro_silence.store(0, Ordering::Relaxed);
                    c.intro_fade.store(0, Ordering::Relaxed);
                }
            }
            // A Stop during cold-start pre-fill leaves reusable buffers but an idle driver.
            // Re-arm the deferred cold start, guarded on !client_started; the normal
            // reuse-while-running path can't double-start the live clock.
            if !self.client_started {
                self.pending_start = true;
                self.pending_since = Some(Instant::now());
            }
        }

        self.adopt_stream_identity(
            event_tx,
            stream_id,
            opened_under,
            sample_rate,
            want_channels,
            duration_secs,
            start_secs,
            consumed,
        );
        self.settle_transport(start_paused, event_tx);
        Ok(())
    }

    /// Adopt a stream's logical identity: ids, format, position baseline, per-stream
    /// flags, and the decoder's throttle cell. Device resources are NOT touched, and a
    /// deferred rebuild adopts these up front; intervening commands then target the
    /// new stream by id while the old device is still fading.
    #[expect(clippy::too_many_arguments)]
    fn adopt_stream_identity(
        &mut self,
        event_tx: &mpsc::Sender<AsioEvent>,
        stream_id: u32,
        opened_under: u32,
        sample_rate: u32,
        want_channels: usize,
        duration_secs: f64,
        start_secs: f64,
        consumed: Arc<AtomicU64>,
    ) {
        self.staging.clear();
        // Adopt the new stream's consumed counter; the superseded stream's cell
        // is abandoned (its decoder is cancelled at spawn time).
        self.consumed = consumed;
        self.stream_id = Some(stream_id);
        self.opened_under = opened_under;
        self.reset_count = 0;
        self.sample_rate = sample_rate;
        self.channels = want_channels;
        self.duration = duration_secs;
        self.baseline_frames = position_frames(start_secs, sample_rate);
        self.stream_ended = false;
        self.drain_since = None;
        let _ = event_tx.send(AsioEvent::Duration {
            stream_id,
            secs: duration_secs,
        });
    }

    /// Apply any latched `pending_transport` Play/Pause (see the field doc for the id-match
    /// rule), re-base position on `played_frames`, and emit the resulting Play/Paused state.
    fn settle_transport(&mut self, start_paused: bool, event_tx: &mpsc::Sender<AsioEvent>) {
        self.played_offset = self.played_frames.load(Ordering::Relaxed);
        self.last_time_report = Instant::now();
        // Consume the latched intent only on id match; a mismatch targets a different,
        // not-yet-adopted stream (a rapid second skip's StartStream can lag this settle),
        // leaving it for that stream's own adoption instead of discarding it here.
        let start_paused = match self.pending_transport {
            Some((id, play)) if self.stream_id == Some(id) => {
                self.pending_transport = None;
                !play
            }
            _ => start_paused,
        };
        self.paused.store(start_paused, Ordering::Relaxed);
        if start_paused {
            self.state = AsioState::Paused;
            let _ = event_tx.send(AsioEvent::StateChange {
                opened_under: self.opened_under,
                state: PlaybackState::Paused,
            });
        } else {
            self.state = AsioState::Playing;
            let _ = event_tx.send(AsioEvent::StateChange {
                opened_under: self.opened_under,
                state: PlaybackState::Active,
            });
        }
    }

    /// Adopt the resources of a freshly built stream and arm the deferred cold start.
    /// `format_changed` arms the resync silence + fade-in masking the DAC PLL relock at
    /// `sample_rate`; a first-stream cold start keeps its plain cushion start (nothing
    /// was playing to pop).
    fn install_stream(&mut self, opened: OpenedAsioStream, format_changed: bool, sample_rate: u32) {
        STREAM_CTX.store(opened.ctx_ptr, Ordering::Release);
        self.ctx_ptr = opened.ctx_ptr;
        self.producer = Some(opened.producer);
        self.ring_capacity = opened.ring_capacity;
        self.frames = opened.frames;
        self.flush_gen = opened.flush_gen;
        self.played_frames = opened.played_frames;
        self.paused = opened.paused;
        self.has_buffers = true;
        // Fresh buffers: the clock is stopped (dispose_stream stopped it). Arm
        // the deferred cold start. On a REUSE track-change the clock keeps running
        // (continuous device hold), and these are NOT reset there: doing so would
        // make maybe_start issue a second driver.start() on the live driver.
        self.client_started = false;
        self.pending_start = true;
        self.pending_since = Some(Instant::now());
        // Set the resync mask before ASIOStart (maybe_start), letting the RT honour it from
        // the first callback.
        if format_changed && !self.ctx_ptr.is_null() {
            let resync = silence_frames(sample_rate, RESYNC_SILENCE_MS);
            // SAFETY: ctx_ptr was just installed and the clock is not started yet
            // (pending_start), leaving no RT callback to race these stores.
            unsafe {
                let c = &*self.ctx_ptr;
                c.intro_silence.store(resync, Ordering::Relaxed);
                c.intro_fade.store(c.fade_len, Ordering::Relaxed);
            }
        }
    }

    fn handle_push_pcm(&mut self, stream_id: u32, samples: Vec<i32>) {
        // Only the adopted stream's PCM is staged; a superseded decoder's samples (id
        // mismatch) are dropped, starving that stale stream's ring to silence.
        if self.stream_id == Some(stream_id) {
            self.staging.extend(samples);
        }
    }

    fn handle_end_stream(&mut self, stream_id: u32) {
        if self.stream_id == Some(stream_id) {
            self.stream_ended = true;
        }
    }

    fn handle_reset_for_seek(
        &mut self,
        event_tx: &mpsc::Sender<AsioEvent>,
        stream_id: u32,
        gen_id: u32,
        start_secs: f64,
    ) {
        if self.stream_id != Some(stream_id) {
            return;
        }
        // Re-base the position and flush stale pre-seek audio. While the clock runs the RT
        // callback drains the ring on the flush_gen bump; while it is stopped (cold/paused) the
        // RT never ticks, and the ring is rebuilt below to evict any already-pumped pre-seek PCM.
        // Mid-rebuild the old ring is left alone: it is mid-fade (a drain would cut the ramp
        // straight to zero, the exact click the fade prevents) and finish_rebuild disposes
        // it wholesale anyway.
        if !matches!(self.state, AsioState::Rebuilding { .. }) {
            self.flush_gen.fetch_add(1, Ordering::Relaxed);
        }
        // Discarded staging counts as consumed, else the throttle would hold
        // the never-played remainder against the decoder forever.
        self.consumed
            .fetch_add(self.staging.len() as u64, Ordering::Relaxed);
        self.staging.clear();
        self.baseline_frames = position_frames(start_secs, self.sample_rate);
        self.played_offset = self.played_frames.load(Ordering::Relaxed);
        self.stream_ended = false;
        self.drain_since = None;

        // Clock stopped: the RT can't drain. Rebuild the ring to evict any pre-seek PCM
        // already pumped into it, then adopt the flush generation on the RT side (else the
        // resume/maybe_start reconcile would mark the stale ring as seen and play it).
        if !self.client_started && !self.ctx_ptr.is_null() {
            let (producer, consumer) = rtrb::RingBuffer::<i32>::new(self.ring_capacity);
            self.producer = Some(producer);
            // SAFETY: client_started == false means the driver clock was never started, and no
            // RT callback is touching the consumer; safe to replace it and the local flush gen.
            unsafe {
                *(*self.ctx_ptr).consumer.get() = consumer;
                (*self.ctx_ptr)
                    .local_flush_gen
                    .store(self.flush_gen.load(Ordering::Relaxed), Ordering::Relaxed);
            }
        }
        // Answer the seek from here rather than leaving it to the periodic report: that
        // report is only emitted by the playing arm; a seek taken in pause would reach
        // no convergence at all and the player would stay pinned until the next Play.
        let _ = event_tx.send(AsioEvent::SeekSettled {
            stream_id,
            gen_id,
            position: start_secs,
            refused: false,
        });
    }

    /// The decoder refused the seek. Answer anyway: an unanswered seek leaves the player
    /// pinned on a target nothing will ever converge to, for the rest of the track. The
    /// position is computed here rather than left to the player, whose own copy still
    /// holds the optimistic target the dispatch wrote before this refusal was known.
    fn handle_seek_failed(&self, event_tx: &mpsc::Sender<AsioEvent>, stream_id: u32, gen_id: u32) {
        if self.stream_id != Some(stream_id) {
            return;
        }
        let _ = event_tx.send(AsioEvent::SeekSettled {
            stream_id,
            gen_id,
            position: self.reported_position_secs(),
            refused: true,
        });
    }

    /// Relay a decoder death to the player. Stream-scoped like the seek answers: a
    /// superseded decoder dying must not retire the track that replaced it.
    fn handle_decode_failed(
        &self,
        event_tx: &mpsc::Sender<AsioEvent>,
        stream_id: u32,
        error: String,
        cause: PacketFailure,
    ) {
        if self.stream_id != Some(stream_id) {
            return;
        }
        let _ = event_tx.send(AsioEvent::DecodeFailed {
            stream_id,
            error,
            cause,
        });
    }

    /// Relay a startup death to the player. Deliberately NOT stream-scoped here, unlike
    /// its sibling: `self.stream_id` can only disagree, because this stream was never
    /// adopted, and reusing that guard would drop every one of these. The player holds the id that
    /// matters; it stamped `current_asio_stream_id` before the spawn, and scopes it there.
    fn handle_startup_failed(
        &self,
        event_tx: &mpsc::Sender<AsioEvent>,
        stream_id: u32,
        error: String,
        cause: PacketFailure,
    ) {
        let _ = event_tx.send(AsioEvent::StartupFailed {
            stream_id,
            error,
            cause,
        });
    }

    /// Baseline plus the frames the RT callback has actually played, clamped to the
    /// track duration. Every position this thread reports comes from here; the counters
    /// only move on a confirmed transition, making it truthful while paused too.
    fn reported_position_secs(&self) -> f64 {
        let played = self
            .played_frames
            .load(Ordering::Relaxed)
            .saturating_sub(self.played_offset);
        let mut pos = (self.baseline_frames + played) as f64 / self.sample_rate.max(1) as f64;
        if self.duration > 0.0 {
            pos = pos.min(self.duration);
        }
        pos
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
            // Adopt the current flush generation on the RT side before starting the clock: a
            // flush_gen bump from a paused initial-seek was never seen by the (stopped) RT, so
            // the first callback would drain the ring, which here holds the correct post-seek
            // audio, not stale pre-seek audio.
            if !self.ctx_ptr.is_null() {
                let cur_gen = self.flush_gen.load(Ordering::Relaxed);
                // SAFETY: ctx_ptr is the leaked StreamCtx, live until dispose_stream; the
                // clock is stopped here (client_started==false), leaving no RT callback to race this.
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
        let _ = event_tx.send(AsioEvent::StateChange {
            opened_under: self.opened_under,
            state: PlaybackState::Active,
        });
    }

    /// Write staged samples into the ring until the ring is full or staging empties.
    fn pump_ring(&mut self) {
        // While the clock is running and an RT flush is still pending, do NOT write: the RT
        // callback drains the ring once when it observes a flush_gen bump (track-change /
        // seek) then sets local_flush_gen. Writing the new track's head before that drain
        // would feed it into the drain and lose it. Gated on `client_started`; a paused
        // cold-start load (clock not yet started, RT never advances local_flush_gen) still
        // fills the ring; the resume reconcile prevents that ring from being drained.
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
            // Ring entries count as consumed for the throttle: the ring bounds
            // itself by capacity, leaving only channel + staging to need the credit.
            self.consumed.fetch_add(written as u64, Ordering::Relaxed);
            if written == 0 {
                break;
            }
        }
    }

    /// Once the pre-fill cushion is buffered, start the driver clock (deferred to keep the
    /// first callbacks from underrunning: the ASIO analogue of wasapi's deferred Start).
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
        // flush_gen while the clock was stopped. Adopt it before starting or the first
        // callback would drain the (correct) post-seek cushion.
        if !self.ctx_ptr.is_null() {
            let cur_gen = self.flush_gen.load(Ordering::Relaxed);
            // SAFETY: ctx_ptr is the leaked StreamCtx, live until dispose_stream; the clock is
            // stopped here (pending_start, client not started), leaving no RT callback to race this.
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
        let _ = event_tx.send(AsioEvent::TimeUpdate(self.reported_position_secs()));
        self.last_time_report = Instant::now();
    }

    /// Complete the track once EOF is reached, staging is drained, and the ring has
    /// emptied, after a short grace that lets the last ASIO double-buffer play out.
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
                // Keep the clock running (continuous hold); the RT emits silence until the
                // next track. flush_gen drops the last drained buffer.
                self.flush_gen.fetch_add(1, Ordering::Relaxed);
                let _ = event_tx.send(AsioEvent::TimeUpdate(self.duration));
                let completed_id = self.stream_id.unwrap_or(0);
                let _ = event_tx.send(AsioEvent::Completed(completed_id));
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
        // A teardown mid-rebuild (shutdown/disconnect/racing switch) must wait the armed fade to
        // silence before ASIOStop, else it cuts the ramp (the click). Bounded by the rebuild's own
        // deadline -> no wait once it landed, no double wait on the stuck-RT/normal-finish paths.
        if let AsioState::Rebuilding { deadline, .. } = self.state
            && !self.ctx_ptr.is_null()
        {
            // SAFETY: ctx_ptr is the live leaked StreamCtx; the RT stores this atomic.
            let ctx = unsafe { &*self.ctx_ptr };
            while !ctx.fade_out_done.load(Ordering::Relaxed) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
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
            // SAFETY: ASIOStop above guarantees no RT callback is in flight, which leaves
            // the leaked StreamCtx box without a live reader and free to reclaim.
            unsafe {
                drop(Box::from_raw(self.ctx_ptr));
            }
            self.ctx_ptr = core::ptr::null_mut();
        }
        self.producer = None;
        self.has_buffers = false;
    }

    /// Drain the PCM still queued in the live ring (the decoded-but-unplayed head)
    /// letting a reset rebuild re-queue it instead of skipping it. ASIOStop first:
    /// no RT callback can race the read; dispose_stream's second stop is a no-op.
    fn drain_ring_leftovers(&mut self) -> Vec<i32> {
        self.stop_driver_if_running();
        if self.ctx_ptr.is_null() {
            return Vec::new();
        }
        // SAFETY: the clock is stopped (no RT callback in flight), which leaves the control
        // thread as the only reader of the leaked StreamCtx until dispose_stream.
        let consumer = unsafe { &mut *(*self.ctx_ptr).consumer.get() };
        let avail = consumer.slots();
        let mut out = Vec::with_capacity(avail);
        if avail > 0
            && let Ok(chunk) = consumer.read_chunk(avail)
        {
            let (s1, s2) = chunk.as_slices();
            out.extend_from_slice(s1);
            out.extend_from_slice(s2);
            chunk.commit_all();
        }
        out
    }

    /// Discard whatever PCM the ring still holds (superseded stream's head).
    /// Only legal while the clock is stopped: then the control thread is the
    /// sole reader of the leaked StreamCtx consumer, same contract as
    /// `drain_ring_leftovers`, but the content is dropped, never restaged.
    fn discard_ring_leftovers(&mut self) {
        self.stop_driver_if_running();
        if self.ctx_ptr.is_null() {
            return;
        }
        // SAFETY: the clock is stopped (no RT callback in flight), which leaves the control
        // thread as the only reader of the leaked StreamCtx.
        let consumer = unsafe { &mut *(*self.ctx_ptr).consumer.get() };
        let avail = consumer.slots();
        if avail > 0
            && let Ok(chunk) = consumer.read_chunk(avail)
        {
            chunk.commit_all();
        }
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
                opened_under,
                sample_rate,
                channels,
                duration_secs,
                start_secs,
                start_paused,
                consumed,
            } => {
                if let AsioState::Rebuilding { deadline, .. } = self.state {
                    // Latest-wins supersede: a rebuild is already pending on the fading
                    // old stream. Re-target its adopted identity and keep the armed
                    // fade + deadline; the single finish_rebuild then builds straight
                    // at the newest format (no second fade, no second build).
                    let want_channels = (channels as usize).clamp(1, CHANNELS);
                    self.adopt_stream_identity(
                        event_tx,
                        stream_id,
                        opened_under,
                        sample_rate,
                        want_channels,
                        duration_secs,
                        start_secs,
                        consumed,
                    );
                    self.state = AsioState::Rebuilding {
                        deadline,
                        start_paused,
                    };
                } else if self
                    .handle_start_stream(
                        event_tx,
                        stream_id,
                        opened_under,
                        sample_rate,
                        channels,
                        duration_secs,
                        start_secs,
                        start_paused,
                        consumed,
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
                gen_id,
                start_secs,
            } => self.handle_reset_for_seek(event_tx, stream_id, gen_id, start_secs),
            AsioCommand::SeekFailed { stream_id, gen_id } => {
                self.handle_seek_failed(event_tx, stream_id, gen_id)
            }
            AsioCommand::DecodeFailed {
                stream_id,
                error,
                cause,
            } => self.handle_decode_failed(event_tx, stream_id, error, cause),
            AsioCommand::StartupFailed {
                stream_id,
                error,
                cause,
            } => self.handle_startup_failed(event_tx, stream_id, error, cause),
            AsioCommand::Play { stream_id } => {
                // Stream-scoped, like PushPcm/EndStream/Completed: a stale Play from a
                // superseded track (the stop->load->play storm) must not resume the wrong
                // stream over the live one.
                if matches!(self.state, AsioState::Rebuilding { .. }) {
                    // Mid-rebuild: latch the intent; settle_transport applies it once this id adopts.
                    self.pending_transport = Some((stream_id, true));
                } else if self.stream_id == Some(stream_id)
                    && matches!(self.state, AsioState::Paused)
                {
                    self.resume(event_tx);
                } else if self.stream_id != Some(stream_id) {
                    // Pre-adoption: this stream's probe-delayed StartStream hasn't
                    // landed yet. Latch the intent; its adoption applies it.
                    self.pending_transport = Some((stream_id, true));
                }
            }
            AsioCommand::Pause { stream_id } => {
                // A stale Pause from the previous track's stop, landing AFTER this track's Play,
                // would re-pause the running clock and wedge the RT in permanent silence
                // (played=0, never underruns). Drop it unless it targets the live stream.
                if matches!(self.state, AsioState::Rebuilding { .. }) {
                    // Mid-rebuild: latch the intent; settle_transport applies it once this id adopts.
                    self.pending_transport = Some((stream_id, false));
                } else if self.stream_id == Some(stream_id)
                    && matches!(self.state, AsioState::Playing)
                {
                    // Hold the clock (KS-exclusive device kept); the RT emits silence while
                    // `paused` is set instead of ASIOStop-ing (which would free the device).
                    self.paused.store(true, Ordering::Relaxed);
                    self.state = AsioState::Paused;
                    let _ = event_tx.send(AsioEvent::StateChange {
                        opened_under: self.opened_under,
                        state: PlaybackState::Paused,
                    });
                } else if self.stream_id != Some(stream_id) {
                    // Pre-adoption pause (e.g. clicked during an auto-play arm):
                    // latch it, making the adoption start paused instead of dropping it.
                    self.pending_transport = Some((stream_id, false));
                }
            }
            AsioCommand::Shutdown => {
                // This span is what a synchronous join would cost its caller.
                let began = std::time::Instant::now();
                self.teardown();
                crate::vprintln!(
                    "[ASIO] teardown: drained in {:.2}ms",
                    began.elapsed().as_secs_f64() * 1000.0
                );
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
                    // Keep the ring topped while paused, making resume glitch-free.
                    self.handle_reset_request(&event_tx);
                    self.pump_ring();
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
                    self.handle_reset_request(&event_tx);
                    self.maybe_start(&event_tx);
                    self.maybe_time_update(&event_tx);
                    self.maybe_complete(&event_tx);
                    self.log_status();
                }
                AsioState::Rebuilding { .. } => {
                    // Same bounded cadence as Playing, keeping queued commands flowing
                    // while the RT fades. The Playing helpers stay off: until
                    // finish_rebuild, self.* mixes the adopted (new) identity with the
                    // old fading device, and pump_ring would feed the new track's head
                    // into the dying ring.
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
                    // Re-read: a superseding StartStream may have replaced the variant's
                    // fields (same deadline, newer start_paused).
                    let AsioState::Rebuilding {
                        deadline,
                        start_paused,
                    } = self.state
                    else {
                        continue;
                    };
                    let fade_done = self.ctx_ptr.is_null() || {
                        // SAFETY: ctx_ptr is the live leaked StreamCtx (valid until
                        // finish_rebuild's dispose_stream); the RT stores this atomic.
                        unsafe { (*self.ctx_ptr).fade_out_done.load(Ordering::Relaxed) }
                    };
                    if fade_done || Instant::now() >= deadline {
                        crate::vprintln!(
                            "[ASIO] deferred rebuild: finishing ({})",
                            if fade_done {
                                "fade complete"
                            } else {
                                "deadline elapsed"
                            }
                        );
                        self.finish_rebuild(start_paused, &event_tx);
                    }
                }
            }
        }
    }

    /// Honor a driver `kAsioResetRequest` (flagged by `asio_message`): ASIO4ALL posts one when
    /// renegotiating its KS pin for a rate change. Rebuild once at the current rate and re-arm
    /// the deferred start; bounded by `reset_count` (a reset loop gives up to `RateUnsupported`:
    /// per-track shared, ASIO stays on). A silent dead clock is caught by the progress watchdog.
    fn handle_reset_request(&mut self, event_tx: &mpsc::Sender<AsioEvent>) {
        if !STREAM_RESET_REQUESTED.swap(false, Ordering::Relaxed) {
            return;
        }
        if self.driver.is_none() || !self.has_buffers {
            return;
        }
        self.reset_count += 1;
        if self.reset_count > 4 {
            crate::vprintln!(
                "[ASIO] reset/rebuild gave up (>4); this track plays shared (ASIO stays on)"
            );
            let _ = event_tx.send(AsioEvent::RateUnsupported {
                stream_id: self.stream_id,
            });
            self.dispose_stream();
            self.state = AsioState::Idle;
            return;
        }
        crate::vprintln!(
            "[ASIO] driver requested reset; rebuilding stream at {} Hz (#{})",
            self.sample_rate,
            self.reset_count
        );
        let sr = self.sample_rate;
        let ch = self.channels;
        // The old ring still holds the decoded-but-unplayed head (up to a full ring).
        // dispose_stream would drop it while pump_ring refills from staging's current
        // point, desyncing the reported position from the audible content. Drain it
        // back (stops the clock, making the position snapshot below exact) and re-queue it.
        let leftovers = self.drain_ring_leftovers();
        // Preserve the audible position across the rebuild. build_stream mints a fresh,
        // zeroed played_frames counter; without carrying the current position forward the
        // reported time freezes at the old baseline (new counter - stale offset saturates
        // to 0), which also trips the progress watchdog into a needless shared fallback.
        let cur_pos_frames = self.baseline_frames.saturating_add(
            self.played_frames
                .load(Ordering::Relaxed)
                .saturating_sub(self.played_offset),
        );
        self.dispose_stream();
        match self.build_stream(self.stream_id, sr, ch) {
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
                // Re-queue the drained head in front of whatever staging held: the new
                // ring then refills starting exactly at the audible position the rebase
                // below reports, keeping content and clock aligned.
                if !leftovers.is_empty() {
                    let mut restored = VecDeque::from(leftovers);
                    restored.append(&mut self.staging);
                    self.staging = restored;
                }
                // Re-base onto the fresh (zeroed) counter, letting position continue from the
                // pre-reset point instead of jumping back to the stream start.
                self.baseline_frames = cur_pos_frames;
                self.played_offset = self.played_frames.load(Ordering::Relaxed);
                // Fresh buffers, clock stopped: arm the deferred cold start.
                self.client_started = false;
                self.pending_start = true;
                self.pending_since = Some(Instant::now());
            }
            Err(ev) => {
                let _ = event_tx.send(ev);
                self.state = AsioState::Idle;
            }
        }
    }

    /// Diagnostic: periodically dump ring/staging/playback state; a mid-playback
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
        let intro = if self.ctx_ptr.is_null() {
            0
        } else {
            // SAFETY: ctx_ptr is the live leaked StreamCtx (valid until dispose_stream); the RT
            // only decrements intro_silence, read here (Relaxed) for the diagnostic.
            unsafe { (*self.ctx_ptr).intro_silence.load(Ordering::Relaxed) }
        };
        crate::vprintln!(
            "[ASIO] status: ring={}/{} staging={} played={} underruns={} switches={} paused={} intro={} ended={} started={}",
            ring_used,
            self.ring_capacity,
            self.staging.len(),
            self.played_frames.load(Ordering::Relaxed),
            STREAM_UNDERRUNS.load(Ordering::Relaxed),
            STREAM_SWITCHES.load(Ordering::Relaxed),
            self.paused.load(Ordering::Relaxed),
            intro,
            self.stream_ended,
            self.client_started,
        );
    }
}

/// Open one enumerated driver and verify it can actually play. The registry lists what is
/// INSTALLED: a driver whose interface is unplugged enumerates identically to a live one,
/// and `init` plus an output-channel count is all that parts them. The channel query here
/// is a presence probe; `build_stream` re-queries per stream and owns the format verdict.
fn try_open_driver(info: &AsioDriverInfo) -> Result<AsioDriver, String> {
    crate::vprintln!("[ASIO] control: opening driver '{}'", info.name);
    // SAFETY: standard COM create + init; the desktop window parents the panel.
    let driver = match unsafe { AsioDriver::create(info.clsid) } {
        Ok(d) => d,
        Err(hr) => return Err(format!("create failed: {hr:#010x}")),
    };
    // SAFETY: the driver was just created, leaving the vtable call live.
    if unsafe { driver.init(GetDesktopWindow()) } == 0 {
        let reason = driver_error_message(&driver).unwrap_or_else(|| "no reason given".into());
        return Err(format!("init failed: {reason}"));
    }
    let (mut ins, mut outs) = (0i32, 0i32);
    // SAFETY: the driver is initialised and both out-params outlive the call.
    if !asio_ok(unsafe { driver.get_channels(&mut ins, &mut outs) }) {
        return Err("getChannels failed".into());
    }
    if outs <= 0 {
        return Err(format!("no output channels ({ins} in / {outs} out)"));
    }
    Ok(driver)
}

fn control_thread(
    gain: Arc<AtomicU32>,
    requested_driver: Option<String>,
    cmd_rx: mpsc::Receiver<AsioCommand>,
    event_tx: mpsc::Sender<AsioEvent>,
) {
    ControlCtx::new(gain, requested_driver).run(cmd_rx, event_tx);
}

/// Player-facing handle to the ASIO control thread. Mirrors `ExclusiveHandle`.
pub(crate) struct AsioHandle {
    cmd_tx: mpsc::Sender<AsioCommand>,
    event_rx: mpsc::Receiver<AsioEvent>,
    thread: Option<JoinHandle<()>>,
}

impl AsioHandle {
    /// Spawn the ASIO control thread. On the first `StartStream` it opens
    /// `requested_driver` when that names an installed driver, else the first
    /// enumerated one that opens. `gain` is the shared digital-gain cell (f32 bits).
    /// `None` when the OS refuses the thread. `thread::spawn` would panic there, on the
    /// player thread that called it, and nothing respawns that one.
    pub(crate) fn spawn(gain: Arc<AtomicU32>, requested_driver: Option<String>) -> Option<Self> {
        let (cmd_tx, cmd_rx) = mpsc::channel::<AsioCommand>();
        let (event_tx, event_rx) = mpsc::channel::<AsioEvent>();
        let thread = match thread::Builder::new()
            .name("asio-control".into())
            .spawn(move || control_thread(gain, requested_driver, cmd_rx, event_tx))
        {
            Ok(t) => t,
            Err(e) => {
                crate::verr!("[ASIO] cannot spawn the control thread: {e}");
                return None;
            }
        };
        Some(Self {
            cmd_tx,
            event_rx,
            thread: Some(thread),
        })
    }

    /// A handle wired to channels the caller owns, with no control thread behind it: a test
    /// can hand the player the very events a driver would have sent. `thread: None` is what
    /// keeps `Drop` from joining one that was never spawned.
    #[cfg(test)]
    pub(crate) fn for_test() -> (Self, mpsc::Sender<AsioEvent>, mpsc::Receiver<AsioCommand>) {
        let (cmd_tx, cmd_rx) = mpsc::channel::<AsioCommand>();
        let (event_tx, event_rx) = mpsc::channel::<AsioEvent>();
        (
            Self {
                cmd_tx,
                event_rx,
                thread: None,
            },
            event_tx,
            cmd_rx,
        )
    }

    pub(crate) fn send(&self, cmd: AsioCommand) {
        let _ = self.cmd_tx.send(cmd);
    }

    /// True once the control thread has stopped, whatever ended it. A FACT, not a timer:
    /// polling it cannot misfire the way a deadline can. Reads the handle without
    /// taking it, leaving `shutdown` and `Drop` free to park or join as before.
    /// Does NOT cover the ASIO real-time callback: it is `extern "system"`, and a panic
    /// there aborts the process before any supervisor could run.
    pub(crate) fn is_dead(&self) -> bool {
        self.thread.as_ref().is_some_and(|t| t.is_finished())
    }

    pub(crate) fn command_sender(&self) -> mpsc::Sender<AsioCommand> {
        self.cmd_tx.clone()
    }

    pub(crate) fn poll_events(&self) -> Vec<AsioEvent> {
        let mut events = Vec::new();
        while let Ok(ev) = self.event_rx.try_recv() {
            events.push(ev);
        }
        events
    }

    /// Send Shutdown and hand back the control thread's JoinHandle WITHOUT
    /// joining: ASIOStop/dispose/Release take as long as the driver wants,
    /// and joining here froze the player command thread. The caller parks the
    /// handle and must see it finish before any ASIO respawn (ASIO loads one
    /// driver at a time).
    #[must_use]
    pub(crate) fn shutdown(mut self) -> Option<JoinHandle<()>> {
        let sent = self.cmd_tx.send(AsioCommand::Shutdown).is_ok();
        let parked = self.thread.take();
        // A dead control thread refuses the send and an already-reaped handle parks
        // nothing: a timing marker has to report what happened, not what was attempted.
        crate::vprintln!(
            "[ASIO] teardown: Shutdown {}, thread {}",
            if sent {
                "sent"
            } else {
                "refused (control thread gone)"
            },
            if parked.is_some() {
                "parked"
            } else {
                "already reaped"
            }
        );
        parked
    }
}

impl Drop for AsioHandle {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(AsioCommand::Shutdown);
        if let Some(h) = self.thread.take() {
            // Whoever drops a live handle pays the join here, park or no park.
            let began = std::time::Instant::now();
            let _ = h.join();
            crate::vprintln!(
                "[ASIO] teardown: joined in Drop, caller blocked {:.2}ms",
                began.elapsed().as_secs_f64() * 1000.0
            );
        }
    }
}

#[cfg(test)]
#[path = "../../../../tests/unit/player/asio/transport_tests.rs"]
mod transport_tests;
