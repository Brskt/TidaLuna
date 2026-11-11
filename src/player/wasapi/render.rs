use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use wasapi::{AudioClient, AudioRenderClient, Handle, SampleType, WaveFormat};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW,
};

use super::endpoint::{convert_pcm_frame, init_exclusive_client, try_open_stream};
use super::{ExclusiveCommand, ExclusiveEvent, StreamStart};
use crate::player::declick::{RESYNC_SILENCE_MS, silence_frames};
use crate::player::packet_failure::PacketFailure;

enum RenderState {
    Idle,
    Playing,
    Paused,
}

/// Absolute frame index a seek/resume to `secs` corresponds to. Used as the
/// `frames_played` baseline for position reporting to stay correct after a seek.
fn position_frames(secs: f64, sample_rate: u32) -> u64 {
    if sample_rate > 0 && secs.is_finite() && secs > 0.0 {
        (secs * sample_rate as f64) as u64
    } else {
        0
    }
}

/// Event wait once the clock runs, and the ceiling for one legitimate render pass.
const EVENT_WAIT_RUNNING_MS: u32 = 50;
/// Event wait while priming: the endpoint event does not fire before the clock starts.
const EVENT_WAIT_PRIMING_MS: u32 = 5;

/// Wall-clock accounting for the render loop: a span outlasting one event wait plus two
/// periods is a stall. An iteration count cannot find one, saying how often the loop turned
/// and never where the time went.
struct StallProbe {
    /// Longest span treated as normal, re-derived from the negotiated period so that a
    /// driver realigning the buffer moves the bar with it.
    threshold: Duration,
    /// Start of the open span, or `None` while the loop is parked on a `recv` whose wait
    /// is unbounded by design.
    since: Option<Instant>,
}

impl StallProbe {
    fn new() -> Self {
        Self {
            threshold: Duration::from_millis(u64::from(EVENT_WAIT_RUNNING_MS)),
            since: None,
        }
    }

    /// One full event wait plus two periods: the longest the loop can legitimately spend
    /// between two laps is a wait that times out followed by a pass of work.
    fn retune(&mut self, buffer_frames: u32, sample_rate: u32) {
        let period_ms = if sample_rate == 0 {
            0
        } else {
            u64::from(buffer_frames) * 1000 / u64::from(sample_rate)
        };
        self.threshold = Duration::from_millis(u64::from(EVENT_WAIT_RUNNING_MS) + 2 * period_ms);
    }

    fn arm(&mut self) {
        self.since = Some(Instant::now());
    }

    /// Close the span without judging it: a loop parked on an unbounded wait is not a
    /// stalled one, and measuring it would report every idle minute as a fault.
    fn park(&mut self) {
        self.since = None;
    }

    /// How long the open span has run, when that is long enough to be a stall. `None`
    /// while parked, which is what keeps an intended wait from reading as one.
    fn overrun(&self, now: Instant) -> Option<Duration> {
        let elapsed = now.duration_since(self.since?);
        (elapsed >= self.threshold).then_some(elapsed)
    }

    fn lap(&mut self, what: &str) {
        let now = Instant::now();
        let overrun = self.overrun(now);
        self.since = Some(now);
        if let Some(elapsed) = overrun {
            crate::vprintln!(
                "[WASAPI] stall: {what} held the render thread {}ms (over {}ms)",
                elapsed.as_millis(),
                self.threshold.as_millis()
            );
        }
    }
}

/// [`StallProbe`] attributes spans of the render loop; this attributes the individual
/// endpoint transitions inside them, which sit in `&self` helpers reached from paths that
/// hold no probe. The bar is the loop's own event-wait ceiling: past it, a driver call is
/// already costing audio.
///
/// `what` is `Display` rather than `&str` so a label composed per call arrives as
/// `format_args!` and stays unformatted for the calls that clear the bar, which is most of them.
pub(super) fn timed_endpoint<T>(what: impl std::fmt::Display, call: impl FnOnce() -> T) -> T {
    let started = Instant::now();
    let out = call();
    let elapsed = started.elapsed();
    if elapsed >= Duration::from_millis(u64::from(EVENT_WAIT_RUNNING_MS)) {
        crate::vprintln!("[WASAPI] endpoint {what} took {}ms", elapsed.as_millis());
    }
    out
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
    // The adopted stream's consumed counter (minted per decoder spawn):
    // credited as bytes are written to the device or discarded; the
    // decoder's sent-minus-consumed throttle stays honest even while this
    // thread is stuck opening the exclusive client.
    consumed: Arc<AtomicU64>,
    current_stream_id: Option<u32>,
    // The load that opened the adopted stream, echoed on its transport states. Written with
    // `current_stream_id`, meaningless without one.
    opened_under: u32,
    // Pre-adoption transport intent (stream_id, play): a Play/Pause for a stream
    // whose probe-delayed StartStream hasn't arrived yet (FIFO makes this
    // deterministic). Every adoption consumes it: applied on id match, discarded
    // otherwise (ids never repeat).
    pending_transport: Option<(u32, bool)>,
    stream_ended: bool,
    last_time_report: Instant,
    state: RenderState,
    // Playing but the audio client is not started yet: held until a PCM cushion
    // is buffered, keeping a seeked start from underrunning into silence (Playing loop).
    pending_start: bool,
    // Whether the WASAPI clock is running. Distinct from pending_start: a fresh
    // start defers start_stream until a buffer of real PCM is pre-filled, and a
    // mid-stream underrun re-arms pending_start with the clock still running.
    client_started: bool,
    // Whether frames reached the endpoint since it was last emptied. Deducing this from
    // the paths that lead here would make a new path silently wrong, and the failure mode
    // is a refused write, not a loud one.
    endpoint_dirty: bool,
    // Frames of post-format-change resync silence the render loop still owes the
    // endpoint before real PCM, to mask the DAC PLL relock at the new rate.
    post_start_silence_remaining: u32,
    // One period of zeros and a reused conversion/padding target, sized per
    // format, leaving the render loop free of allocation.
    silence_buf: Vec<u8>,
    scratch: Vec<u8>,
}

impl RenderContext {
    fn new() -> Self {
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
            consumed: Arc::new(AtomicU64::new(0)),
            current_stream_id: None,
            opened_under: 0,
            pending_transport: None,
            stream_ended: true,
            last_time_report: Instant::now(),
            state: RenderState::Idle,
            pending_start: false,
            client_started: false,
            endpoint_dirty: false,
            post_start_silence_remaining: 0,
            silence_buf: Vec::new(),
            scratch: Vec::new(),
        }
    }

    /// Whether the endpoint owes a `Reset()`. A running clock drains it a period at a
    /// time; only a stopped one can hand the pre-Start write a full buffer and play
    /// queued pre-seek frames on restart, and only if something reached it at all.
    fn owes_flush(&self) -> bool {
        !self.client_started && self.endpoint_dirty
    }

    fn stop_audio_client(&self) {
        if let Some(ref ac) = self.audio_client {
            let _ = timed_endpoint("stop (teardown)", || ac.stop_stream());
        }
    }

    /// Release the exclusive device on a real stop: drop the IAudioClient for other
    /// apps to regain the endpoint, then park in Idle. The next StartStream reopens it
    /// (handle_start_stream sees `audio_client == None` and opens a fresh client).
    fn release_device(&mut self) {
        self.stop_audio_client();
        self.render_client = None;
        self.h_event = None;
        self.audio_client = None;
        // Discarded unplayed audio counts as consumed for the decoder's throttle.
        self.consumed.fetch_add(
            self.pcm_data.len().saturating_sub(self.write_cursor) as u64,
            Relaxed,
        );
        self.pcm_data.clear();
        self.write_cursor = 0;
        self.frames_played = 0;
        self.current_stream_id = None;
        self.pending_transport = None;
        self.stream_ended = true;
        self.client_started = false;
        // The client was dropped above, and the frames it held went with it.
        self.endpoint_dirty = false;
        self.pending_start = false;
        self.post_start_silence_remaining = 0;
        self.state = RenderState::Idle;
    }

    /// Open a new stream, reset all PCM/playback state, start playback.
    /// Postconditions on Ok: pcm_data cleared, cursors zeroed, stream_id set,
    /// audio resources replaced, state = Playing, Duration + Active events sent.
    /// On Err: an error event was sent and render_thread must exit.
    fn handle_start_stream(
        &mut self,
        device: &wasapi::Device,
        event_tx: &mpsc::Sender<ExclusiveEvent>,
        start: StreamStart,
    ) -> Result<(), ()> {
        let StreamStart {
            stream_id,
            sample_rate,
            channels,
            bits_per_sample,
            ..
        } = start;
        // Reuse the open client on an unchanged format (reopening pops the DAC).
        // A format change needs a fresh one, but an exclusive client owns the device.
        // Release the old one first (else the new initialize fails with
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
                stream_id,
                sample_rate,
                channels,
                bits_per_sample,
                event_tx,
            )?;

            // Sized once per format, keeping the per-period path allocation-free.
            let period_bytes =
                bs as usize * (wf.get_bitspersample() as usize / 8) * channels as usize;
            self.silence_buf = vec![0u8; period_bytes];
            self.scratch = Vec::with_capacity(period_bytes);

            self.audio_client = Some(ac);
            self.render_client = Some(rc);
            self.h_event = Some(ev);
            self.wave_fmt = Some(wf);
            self.buffer_size = bs;
            // A freshly initialized endpoint has never been written to.
            self.endpoint_dirty = false;
            // Mask the DAC PLL relock at the new rate with a resync silence (shared sizing
            // with the ASIO backend).
            self.post_start_silence_remaining = if had_client {
                silence_frames(sample_rate, RESYNC_SILENCE_MS) as u32
            } else {
                0
            };
        } else if let Some(ref ac) = self.audio_client {
            // Reusing the open client: the clock stops either way, letting the start
            // branch below re-prime it, but only frames actually queued are worth the
            // Reset() that drops them.
            let _ = timed_endpoint("stop (format reuse)", || ac.stop_stream());
            if self.endpoint_dirty {
                let _ = timed_endpoint("reset (format reuse)", || ac.reset_stream());
                self.endpoint_dirty = false;
            }
            // Same-format reuse: continuous endpoint, no relock -> no resync silence
            // (clear any leftover from a just-prior rate change).
            self.post_start_silence_remaining = 0;
        }

        self.adopt_stream(event_tx, start);
        Ok(())
    }

    /// The adoption itself, apart from the device work `handle_start_stream` does before it:
    /// what the context holds of the stream, and the Duration and transport state it announces.
    /// Opens no device: a paused start only stops the client already held, none in a test's.
    fn adopt_stream(&mut self, event_tx: &mpsc::Sender<ExclusiveEvent>, start: StreamStart) {
        let StreamStart {
            stream_id,
            opened_under,
            sample_rate,
            channels,
            bits_per_sample,
            duration_secs,
            start_secs,
            start_paused,
            consumed,
        } = start;
        self.pcm_data.clear();
        self.pcm_sample_rate = sample_rate;
        self.pcm_channels = channels;
        self.pcm_src_bps = bits_per_sample;
        self.pcm_duration = duration_secs;
        self.write_cursor = 0;
        // Adopt the new stream's consumed counter; the superseded stream's cell
        // is abandoned (its decoder is cancelled at spawn time).
        self.consumed = consumed;
        self.frames_played = position_frames(start_secs, sample_rate);
        self.current_stream_id = Some(stream_id);
        self.opened_under = opened_under;
        self.stream_ended = false;
        self.last_time_report = Instant::now();
        // Apply a transport command that raced ahead of this adoption (the player
        // can send Play/Pause for this stream before its probe-delayed StartStream
        // lands). Consumed on EVERY adoption: applied on id match, else discarded.
        let start_paused = match self.pending_transport.take() {
            Some((id, play)) if id == stream_id => !play,
            _ => start_paused,
        };

        let _ = event_tx.send(ExclusiveEvent::Duration {
            stream_id,
            secs: duration_secs,
        });
        // Do NOT start the clock yet: per IAudioClient::Start the buffer must
        // hold real PCM first, else the tiny exclusive buffer underruns at the
        // silence->audio edge (saturation). The Playing loop pre-fills, then starts.
        self.client_started = false;
        if start_paused {
            // A paused load has an empty endpoint; arm the deferred start: Play
            // pre-fills before starting, rather than start_stream() on empty.
            self.pending_start = true;
            self.stop_audio_client();
            self.state = RenderState::Paused;
            let _ = event_tx.send(ExclusiveEvent::StateChange {
                opened_under,
                state: crate::player::PlaybackState::Paused,
            });
        } else {
            self.pending_start = true;
            self.state = RenderState::Playing;
            let _ = event_tx.send(ExclusiveEvent::StateChange {
                opened_under,
                state: crate::player::PlaybackState::Active,
            });
        }
    }

    fn handle_push_pcm(&mut self, stream_id: u32, data: Vec<u8>) {
        if self.current_stream_id == Some(stream_id) {
            // Reclaim the played prefix before appending: with the decode
            // throttle this bounds pcm_data to the decode-ahead target.
            if self.write_cursor > 0 {
                self.pcm_data.drain(..self.write_cursor);
                self.write_cursor = 0;
            }
            self.pcm_data.extend_from_slice(&data);
        }
    }

    /// What a `Play` means wherever it reaches the render loop. The stream id decides first; one
    /// for a stream not yet adopted is latched for its own `StartStream`, never applied to the
    /// context currently armed.
    ///
    /// The running state is read here rather than presumed by the caller. The drain serving
    /// `Playing` loses it mid-burst whenever a `Pause` and the `Play` answering it reach one
    /// `try_recv` sweep, and a caller still holding "we are playing" reads that pair as a no-op.
    fn apply_play(&mut self, event_tx: &mpsc::Sender<ExclusiveEvent>, stream_id: u32) {
        if self.current_stream_id != Some(stream_id) {
            // Latched, NOT resumed: only the matching adoption may apply it, and an id that
            // never matches leaves playback waiting on a user action.
            crate::vprintln!(
                "[WASAPI] Play latched for stream {stream_id}, current is {:?}",
                self.current_stream_id
            );
            self.pending_transport = Some((stream_id, true));
            return;
        }
        if matches!(self.state, RenderState::Playing) {
            return;
        }
        crate::vprintln!("[WASAPI] render resumed by Play (stream {stream_id})");
        // The pause armed `pending_start` and flushed a dirty endpoint: the playing loop
        // re-primes a full period ahead of Start() rather than restarting over a stale tail.
        self.state = RenderState::Playing;
        let _ = event_tx.send(ExclusiveEvent::StateChange {
            opened_under: self.opened_under,
            state: crate::player::PlaybackState::Active,
        });
    }

    /// What a `Pause` means to the playing render loop, apart from `apply_play` for the seam
    /// it gives a test. Stream-scoped: a stale pause from a superseded track must not stop the
    /// live stream; one for a not-yet-adopted stream is latched, making its StartStream start
    /// paused.
    fn apply_pause(&mut self, event_tx: &mpsc::Sender<ExclusiveEvent>, stream_id: u32) {
        if self.current_stream_id != Some(stream_id) {
            self.pending_transport = Some((stream_id, false));
        } else {
            // The clock stops for real here, and `pending_start` makes
            // resume re-prime a full period before Start(): a bare restart
            // over a stale tail saturates. A pause before the first write
            // has nothing queued, and only a tail that exists is worth a
            // Reset().
            if let Some(ref ac) = self.audio_client {
                let _ = timed_endpoint("stop (pause)", || ac.stop_stream());
                if self.endpoint_dirty {
                    let _ = timed_endpoint("reset (pause)", || ac.reset_stream());
                    self.endpoint_dirty = false;
                }
            }
            self.client_started = false;
            self.pending_start = true;
            self.state = RenderState::Paused;
            // From here the loop blocks on `recv` and nothing downstream
            // reports again until a command wakes it. Only `release_device`
            // hands the device back, never a pause.
            crate::vprintln!("[WASAPI] render paused, endpoint still held (stream {stream_id})");
            let _ = event_tx.send(ExclusiveEvent::StateChange {
                opened_under: self.opened_under,
                state: crate::player::PlaybackState::Paused,
            });
        }
    }

    fn handle_end_stream(&mut self, stream_id: u32) {
        if self.current_stream_id == Some(stream_id) {
            self.stream_ended = true;
        }
    }

    fn handle_reset_for_seek(
        &mut self,
        event_tx: &mpsc::Sender<ExclusiveEvent>,
        stream_id: u32,
        gen_id: u32,
        start_secs: f64,
    ) {
        if self.current_stream_id != Some(stream_id) {
            // Silently dropping this looks exactly like a render thread too busy to answer,
            // and the two have opposite causes.
            crate::vprintln!(
                "[WASAPI] seek reset dropped: for stream {stream_id}, current is {:?}",
                self.current_stream_id
            );
            return;
        }
        // Splits the gap between the decoder's `live seek` and the return to active: this
        // line is the render side receiving it, the cushion line below is real PCM
        // resuming. Without both, that interval is one unattributed block.
        crate::vprintln!("[WASAPI] seek reset received at {start_secs:.1}s");
        if self.owes_flush()
            && let Some(ref ac) = self.audio_client
        {
            let _ = timed_endpoint("stop (seek)", || ac.stop_stream());
            let _ = timed_endpoint("reset (seek)", || ac.reset_stream());
            self.endpoint_dirty = false;
        }
        // Discarded unplayed audio counts as consumed for the decoder's throttle.
        self.consumed.fetch_add(
            self.pcm_data.len().saturating_sub(self.write_cursor) as u64,
            Relaxed,
        );
        self.pcm_data.clear();
        self.write_cursor = 0;
        self.frames_played = position_frames(start_secs, self.pcm_sample_rate);
        self.stream_ended = false;
        // Re-arm the cushion: the loop feeds the endpoint silence until it fills. On the
        // stopped-clock path it also holds Start() back, an unfilled exclusive buffer
        // there being what made seeks saturate.
        self.pending_start = true;
        self.last_time_report = Instant::now();
        // Answer the seek from here rather than leaving it to the periodic report: that
        // report is only emitted by the playing arm; a seek taken in pause would reach
        // no convergence at all and the player would stay pinned until the next Play.
        let _ = event_tx.send(ExclusiveEvent::SeekSettled {
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
    fn handle_seek_failed(
        &self,
        event_tx: &mpsc::Sender<ExclusiveEvent>,
        stream_id: u32,
        gen_id: u32,
    ) {
        if self.current_stream_id != Some(stream_id) {
            return;
        }
        let _ = event_tx.send(ExclusiveEvent::SeekSettled {
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
        event_tx: &mpsc::Sender<ExclusiveEvent>,
        stream_id: u32,
        error: String,
        cause: PacketFailure,
    ) {
        if self.current_stream_id != Some(stream_id) {
            return;
        }
        let _ = event_tx.send(ExclusiveEvent::DecodeFailed {
            stream_id,
            error,
            cause,
        });
    }

    /// Relay a startup death to the player. Not stream-scoped here, unlike its sibling:
    /// `current_stream_id` can only disagree on a stream never adopted, and that guard would
    /// drop every one of these. The player stamped the id that matters, and scopes it there.
    fn handle_startup_failed(
        &self,
        event_tx: &mpsc::Sender<ExclusiveEvent>,
        stream_id: u32,
        error: String,
        cause: PacketFailure,
    ) {
        let _ = event_tx.send(ExclusiveEvent::StartupFailed {
            stream_id,
            error,
            cause,
        });
    }

    /// Position backed by audible audio: clamped to the frames actually covered by
    /// buffered PCM, never reporting ahead of what can be heard on a forward seek into
    /// a still-downloading region. Every position this thread reports comes from here.
    fn reported_position_secs(&self) -> f64 {
        if self.pcm_sample_rate == 0 {
            return 0.0;
        }
        let src_bytes_per_frame = (self.pcm_src_bps / 8) as usize * self.pcm_channels as usize;
        let consumed_frames = self
            .write_cursor
            .checked_div(src_bytes_per_frame)
            .unwrap_or(0) as u64;
        let buffered_frames = self
            .pcm_data
            .len()
            .checked_div(src_bytes_per_frame)
            .unwrap_or(0) as u64;
        let baseline = self.frames_played.saturating_sub(consumed_frames);
        self.frames_played.min(baseline + buffered_frames) as f64 / self.pcm_sample_rate as f64
    }
}

/// RAII guard registering the render thread with the MMCSS "Pro Audio" task, which
/// wakes it on the device period. The default timer granularity wakes too late and
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

pub(super) fn render_thread(
    device_id: String,
    cmd_rx: mpsc::Receiver<ExclusiveCommand>,
    event_tx: mpsc::Sender<ExclusiveEvent>,
    gain: Arc<AtomicU32>,
) {
    let _ = render_thread_inner(device_id, cmd_rx, event_tx, gain);
}

fn render_thread_inner(
    device_id: String,
    cmd_rx: mpsc::Receiver<ExclusiveCommand>,
    event_tx: mpsc::Sender<ExclusiveEvent>,
    gain: Arc<AtomicU32>,
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

    // MMCSS "Pro Audio": wake on the device period, not the coarse system timer. Held
    // for the render loop (reverted on drop).
    let _mmcss = ProAudioMmcss::register();

    let mut ctx = RenderContext::new();
    let mut diag_logged = false;
    // Startup render-loop diagnostic (LOGS=3): classify each of the first 150
    // Playing iterations to confirm/refute a buffer underrun at startup. Cheap
    // per-iteration counters, a single dump at the end (low observer effect).
    let mut diag_it = 0u32;
    let mut diag_done = false;
    // Iterations spent waiting for the cushion, reset at each start. Sampling this curve
    // tells a steady trickle of PCM apart from one late block, which have opposite causes.
    let mut buf_it = 0u32;
    // The two early exits that skip the cushion check entirely: a render state other than
    // Playing, and an endpoint with no room. Both leave the checkpoints below silent.
    let mut idle_it = 0u32;
    let mut zero_it = 0u32;
    let mut diag_last = Instant::now();
    let mut diag_max_gap = 0u128;
    let mut diag_late = 0u32;
    let (mut diag_s, mut diag_u, mut diag_p, mut diag_f) = (0u32, 0u32, 0u32, 0u32);
    let mut diag_min_rem = usize::MAX;

    let mut stall = StallProbe::new();
    loop {
        // Every `continue` in the arms below lands here; this lap is what makes the
        // accounting exhaustive: a call nobody wrapped is still reported, charged to the
        // pass instead of to itself.
        stall.retune(ctx.buffer_size, ctx.pcm_sample_rate);
        stall.lap("render pass");
        match ctx.state {
            RenderState::Idle => {
                // Unbounded by design: with no stream adopted the next command can be
                // minutes away. The wait itself is excluded; only the handling of what it
                // returns is measured.
                stall.park();
                let cmd = cmd_rx.recv();
                stall.arm();
                let handler = match &cmd {
                    Ok(c) => c.label(),
                    Err(_) => "disconnect",
                };
                match cmd {
                    Ok(ExclusiveCommand::StartStream(start)) => {
                        crate::vprintln!(
                            "[WASAPI-DBG] render: StartStream received, opening exclusive client"
                        );
                        ctx.handle_start_stream(&device, &event_tx, start)?;
                    }
                    Ok(ExclusiveCommand::ReleaseDevice) => ctx.release_device(),
                    Ok(ExclusiveCommand::Shutdown) | Err(_) => break,
                    // Play/Pause come from the command thread, not the decoder, and can arrive
                    // before the probe-delayed StartStream they belong to. Idle is where that
                    // race is certain: dropping them started a track after a pause request.
                    Ok(ExclusiveCommand::Play { stream_id }) => {
                        ctx.apply_play(&event_tx, stream_id)
                    }
                    Ok(ExclusiveCommand::Pause { stream_id }) => {
                        if ctx.current_stream_id != Some(stream_id) {
                            ctx.pending_transport = Some((stream_id, false));
                        }
                    }
                    // Idle is where a startup death is EXPECTED to land: the decoder gave up
                    // before the StartStream that would have left this state. Relaying it is
                    // the whole point, so it sits above the dropped group rather than in it.
                    Ok(ExclusiveCommand::StartupFailed {
                        stream_id,
                        error,
                        cause,
                    }) => ctx.handle_startup_failed(&event_tx, stream_id, error, cause),
                    // The rest is decoder-sent AFTER its own StartStream (`StartupFailed` is
                    // the sole one that precedes it, hence handled just above), so one
                    // reaching idle names a stream already gone. Named rather than
                    // wildcarded, so a new variant is triaged here instead of vanishing.
                    Ok(ExclusiveCommand::PushPcm { .. })
                    | Ok(ExclusiveCommand::EndStream { .. })
                    | Ok(ExclusiveCommand::ResetForSeek { .. })
                    | Ok(ExclusiveCommand::SeekFailed { .. })
                    | Ok(ExclusiveCommand::DecodeFailed { .. }) => {}
                }
                stall.lap(handler);
            }

            RenderState::Playing => {
                while let Ok(cmd) = cmd_rx.try_recv() {
                    // Without a lap per command, a handler that overruns is
                    // indistinguishable from a thread blocked on a syscall.
                    let handler = cmd.label();
                    match cmd {
                        ExclusiveCommand::Play { stream_id } => {
                            ctx.apply_play(&event_tx, stream_id)
                        }
                        ExclusiveCommand::Pause { stream_id } => {
                            ctx.apply_pause(&event_tx, stream_id)
                        }
                        ExclusiveCommand::StartStream(start) => {
                            ctx.handle_start_stream(&device, &event_tx, start)?;
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
                            gen_id,
                            start_secs,
                        } => ctx.handle_reset_for_seek(&event_tx, stream_id, gen_id, start_secs),
                        ExclusiveCommand::SeekFailed { stream_id, gen_id } => {
                            ctx.handle_seek_failed(&event_tx, stream_id, gen_id)
                        }
                        ExclusiveCommand::DecodeFailed {
                            stream_id,
                            error,
                            cause,
                        } => ctx.handle_decode_failed(&event_tx, stream_id, error, cause),
                        ExclusiveCommand::StartupFailed {
                            stream_id,
                            error,
                            cause,
                        } => ctx.handle_startup_failed(&event_tx, stream_id, error, cause),
                        ExclusiveCommand::ReleaseDevice => ctx.release_device(),
                        ExclusiveCommand::Shutdown => {
                            ctx.stop_audio_client();
                            return Ok(());
                        }
                    }
                    stall.lap(handler);
                }

                if !matches!(ctx.state, RenderState::Playing) {
                    // This `continue` skips the whole loop body: a seek waiting on a state
                    // change leaves no trace at any of the checkpoints below.
                    if idle_it.is_multiple_of(100) {
                        crate::vprintln3!(
                            "[WASAPI-DIAG] render parked it={idle_it} state={}",
                            match ctx.state {
                                RenderState::Idle => "Idle",
                                RenderState::Playing => "Playing",
                                RenderState::Paused => "Paused",
                            }
                        );
                    }
                    idle_it += 1;
                    continue;
                }
                idle_it = 0;

                // Before the clock starts the event never fires; poll briefly to
                // build the cushion. Once running it fires each period (capped).
                if let Some(ref ev) = ctx.h_event {
                    let timeout_ms = if ctx.client_started {
                        EVENT_WAIT_RUNNING_MS
                    } else {
                        EVENT_WAIT_PRIMING_MS
                    };
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
                        // The other early exit that hides the cushion check: with the clock
                        // stopped nothing drains the endpoint, and this can persist.
                        if zero_it.is_multiple_of(100) {
                            crate::vprintln3!(
                                "[WASAPI-DIAG] endpoint full it={zero_it} clock_started={}",
                                ctx.client_started
                            );
                        }
                        zero_it += 1;
                        continue;
                    }
                    zero_it = 0;

                    let src_bytes_per_sample = (ctx.pcm_src_bps / 8) as usize;
                    let src_bytes_per_frame = src_bytes_per_sample * ctx.pcm_channels as usize;
                    let dst_bytes_per_sample = wf.get_bitspersample() as usize / 8;
                    let dst_bytes_per_frame = dst_bytes_per_sample * ctx.pcm_channels as usize;

                    // Post-format-change resync silence: hold the endpoint at zero while the
                    // DAC PLL relocks at the new rate. Emits silence without consuming pcm_data,
                    // delaying the track head rather than dropping it.
                    if ctx.post_start_silence_remaining > 0 {
                        let _ = rc.write_to_device(
                            &ctx.silence_buf[..available * dst_bytes_per_frame],
                            None,
                        );
                        ctx.endpoint_dirty = true;
                        ctx.post_start_silence_remaining = ctx
                            .post_start_silence_remaining
                            .saturating_sub(available as u32);
                        if !ctx.client_started {
                            // Pre-roll is in the endpoint; starting the clock plays it out and
                            // frees space for the next silence buffer (exclusive event mode
                            // needs a buffer written before Start()).
                            let _ = timed_endpoint("start (resync silence)", || ac.start_stream());
                            ctx.client_started = true;
                        }
                        continue;
                    }

                    let remaining_src_bytes = ctx.pcm_data.len().saturating_sub(ctx.write_cursor);
                    let remaining_frames = remaining_src_bytes
                        .checked_div(src_bytes_per_frame)
                        .unwrap_or(0);

                    // Cushion: hold the deferred start until half a second is buffered (or
                    // the stream ends). A fresh start just accumulates; a mid-stream
                    // underrun feeds the still-running client silence below.
                    let buffering = ctx.pending_start
                        && !ctx.stream_ended
                        && remaining_frames < ctx.pcm_sample_rate as usize / 2;
                    if ctx.pending_start && !buffering {
                        ctx.pending_start = false;
                        // Which of the two refills this was: a seek that kept the clock
                        // running never starts anything.
                        crate::vprintln!(
                            "[WASAPI] cushion filled after {buf_it} passes: {remaining_frames} frames ({:.0}ms), {}{}",
                            remaining_frames as f64 * 1000.0 / ctx.pcm_sample_rate.max(1) as f64,
                            if ctx.client_started {
                                "clock already running"
                            } else {
                                "clock starting"
                            },
                            if ctx.stream_ended {
                                " (stream ended)"
                            } else {
                                ""
                            }
                        );
                        buf_it = 0;
                    }

                    if !diag_done && ctx.client_started {
                        let gap = diag_last.elapsed().as_millis();
                        diag_last = Instant::now();
                        if diag_it > 0 {
                            diag_max_gap = diag_max_gap.max(gap);
                            // Late = >1.5x the 20ms device period (a wake that
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
                        // Every 20th pass. The loop spins on a 5ms timeout here; logging
                        // each one would bury the capture it is meant to explain.
                        if buf_it.is_multiple_of(20) {
                            crate::vprintln3!(
                                "[WASAPI-DIAG] buffering it={buf_it} remaining={remaining_frames} frames"
                            );
                        }
                        buf_it += 1;
                        if ctx.stream_ended && remaining_frames == 0 {
                            // Stream ended before any audio decoded: complete now.
                            let _ = event_tx.send(ExclusiveEvent::TimeUpdate(ctx.pcm_duration));
                            match ctx.current_stream_id {
                                Some(stream_id) => {
                                    let _ = event_tx.send(ExclusiveEvent::Completed(stream_id));
                                }
                                // Every path into `Playing` sets the id and every reset of it
                                // sets `Idle`, so this is a broken invariant rather than a case
                                // to fold: an unnamed completion would be charged to whichever
                                // stream is current when it lands.
                                None => crate::verr!(
                                    "[WASAPI] completion while Playing with no stream id"
                                ),
                            }
                            ctx.stop_audio_client();
                            ctx.current_stream_id = None;
                            ctx.stream_ended = true;
                            ctx.state = RenderState::Idle;
                        }
                        continue;
                    }

                    if remaining_frames == 0 || buffering {
                        // A mid-stream underrun (no PCM, stream not ended): re-arm
                        // the start cushion. The next refill rebuilds a buffer
                        // before resuming, preventing a repeated stutter.
                        if remaining_frames == 0 && !ctx.stream_ended {
                            ctx.pending_start = true;
                        }
                        let _ = rc.write_to_device(
                            &ctx.silence_buf[..available * dst_bytes_per_frame],
                            None,
                        );
                        ctx.endpoint_dirty = true;
                        // The running-clock refill counts too: without this the cushion line
                        // reports a count only the stopped-clock path ever measured.
                        buf_it += 1;

                        if buffering || !ctx.stream_ended {
                            continue;
                        }

                        let _ = event_tx.send(ExclusiveEvent::TimeUpdate(ctx.pcm_duration));
                        match ctx.current_stream_id {
                            Some(stream_id) => {
                                let _ = event_tx.send(ExclusiveEvent::Completed(stream_id));
                            }
                            // Same broken invariant as the drained-early branch above: loud,
                            // never folded onto whichever stream happens to be current.
                            None => {
                                crate::verr!("[WASAPI] completion while Playing with no stream id")
                            }
                        }

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

                    let full_bytes = available * dst_bytes_per_frame;
                    let passthrough = ctx.pcm_src_bps == dst_store
                        && matches!(dst_type, SampleType::Int)
                        && g >= 1.0;
                    let write_slice: &[u8] = if passthrough && src_chunk.len() == full_bytes {
                        src_chunk
                    } else {
                        if passthrough {
                            ctx.scratch.clear();
                            ctx.scratch.extend_from_slice(src_chunk);
                        } else {
                            convert_pcm_frame(
                                src_chunk,
                                ctx.pcm_src_bps,
                                dst_store,
                                dst_valid,
                                &dst_type,
                                ctx.pcm_channels,
                                g,
                                &mut ctx.scratch,
                            );
                        }
                        // Exclusive event mode requires full-buffer packets: zero-pad
                        // a short tail (track end) in place, else the write returns
                        // AUDCLNT_E_BUFFER_SIZE_ERROR and drops the last frames.
                        ctx.scratch.resize(full_bytes, 0);
                        &ctx.scratch
                    };

                    if !diag_logged {
                        diag_logged = true;
                        let (mut mn, mut mx) = (i16::MAX, i16::MIN);
                        if dst_store == 16 {
                            for s in write_slice.as_chunks::<2>().0 {
                                let v = i16::from_le_bytes(*s);
                                mn = mn.min(v);
                                mx = mx.max(v);
                            }
                        }
                        let head: Vec<String> = write_slice
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

                    // Split the pass around the endpoint write, the one call here that
                    // reaches the driver: charged to a whole pass it would be one more
                    // unattributed span.
                    stall.lap("pass up to write");
                    if let Err(e) = rc.write_to_device(write_slice, None) {
                        crate::vprintln!("[WASAPI] write error: {e}");
                    }
                    // Set even on failure: whether frames landed is unknown, and a missed
                    // Reset() plays a stale tail where a needless one only costs time.
                    ctx.endpoint_dirty = true;
                    stall.lap("write_to_device");

                    // Advance by the real frames only; the silence padding is not
                    // part of the decoded stream or the playback position.
                    ctx.write_cursor += src_chunk_size;
                    ctx.frames_played += frames_to_write as u64;
                    ctx.consumed.fetch_add(src_chunk_size as u64, Relaxed);

                    if !ctx.client_started {
                        // First real buffer is now loaded -> start the clock once
                        // the audio-client borrow is released (after this if-let).
                        start_clock_now = true;
                    }

                    if ctx.last_time_report.elapsed().as_millis() >= 200 {
                        let _ =
                            event_tx.send(ExclusiveEvent::TimeUpdate(ctx.reported_position_secs()));
                        ctx.last_time_report = Instant::now();
                    }
                }

                if start_clock_now {
                    // The endpoint is pre-filled. Start the clock, then reset the diag
                    // timer, keeping the first gap on steady-state, not the cushion poll.
                    if let Some(ref ac) = ctx.audio_client {
                        let _ = timed_endpoint("start (clock)", || ac.start_stream());
                    }
                    stall.lap("pass after write");
                    ctx.client_started = true;
                    diag_last = Instant::now();
                }
            }

            RenderState::Paused => {
                // Unbounded by design, like the Idle arm: a paused render waits on a user action.
                stall.park();
                let cmd = cmd_rx.recv();
                stall.arm();
                let handler = match &cmd {
                    Ok(c) => c.label(),
                    Err(_) => "disconnect",
                };
                match cmd {
                    Ok(ExclusiveCommand::Play { stream_id }) => {
                        // Falls through on the latched branch, like the Playing arm's Pause:
                        // an early exit would leave the probe's span open.
                        ctx.apply_play(&event_tx, stream_id)
                    }
                    Ok(ExclusiveCommand::Pause { stream_id }) => {
                        // Already paused; only a pause for a not-yet-adopted stream
                        // matters. Latching it makes its StartStream start paused.
                        if ctx.current_stream_id != Some(stream_id) {
                            ctx.pending_transport = Some((stream_id, false));
                        }
                    }
                    Ok(ExclusiveCommand::StartStream(start)) => {
                        ctx.handle_start_stream(&device, &event_tx, start)?;
                    }
                    Ok(ExclusiveCommand::PushPcm {
                        stream_id,
                        pcm_data: data,
                    }) => ctx.handle_push_pcm(stream_id, data),
                    Ok(ExclusiveCommand::EndStream { stream_id }) => {
                        ctx.handle_end_stream(stream_id)
                    }
                    Ok(ExclusiveCommand::ResetForSeek {
                        stream_id,
                        gen_id,
                        start_secs,
                    }) => ctx.handle_reset_for_seek(&event_tx, stream_id, gen_id, start_secs),
                    Ok(ExclusiveCommand::SeekFailed { stream_id, gen_id }) => {
                        ctx.handle_seek_failed(&event_tx, stream_id, gen_id)
                    }
                    Ok(ExclusiveCommand::DecodeFailed {
                        stream_id,
                        error,
                        cause,
                    }) => ctx.handle_decode_failed(&event_tx, stream_id, error, cause),
                    Ok(ExclusiveCommand::StartupFailed {
                        stream_id,
                        error,
                        cause,
                    }) => ctx.handle_startup_failed(&event_tx, stream_id, error, cause),
                    Ok(ExclusiveCommand::ReleaseDevice) => ctx.release_device(),
                    Ok(ExclusiveCommand::Shutdown) | Err(_) => {
                        ctx.stop_audio_client();
                        return Ok(());
                    }
                }
                stall.lap(handler);
            }
        }
    }

    Ok(())
}

#[cfg(test)]
#[path = "../../../tests/unit/player/wasapi/pcm_compaction_tests.rs"]
mod pcm_compaction_tests;

#[cfg(test)]
#[path = "../../../tests/unit/player/wasapi/endpoint_flush_tests.rs"]
mod endpoint_flush_tests;

#[cfg(test)]
#[path = "../../../tests/unit/player/wasapi/stall_probe_tests.rs"]
mod stall_probe_tests;

#[cfg(test)]
#[path = "../../../tests/unit/player/wasapi/transport_tests.rs"]
mod transport_tests;
