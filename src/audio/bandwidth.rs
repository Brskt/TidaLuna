use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::time::Instant;
use tokio::sync::{mpsc, oneshot};
use tokio::time::{self, MissedTickBehavior};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrafficClass {
    Playback,
    Preload,
}

/// Shared atomic counters updated by StreamingBuffer read/write operations.
/// The governor reads these on every tick (25 ms) to compute hysteresis.
#[derive(Default)]
pub struct BufferProgress {
    pub written: AtomicU64,
    pub read_pos: AtomicU64,
    pub total_len: AtomicU64,
    pub bitrate_bps: AtomicU64,
    /// true when the player is actively playing (not paused/stopped).
    playback_active: AtomicBool,
    /// Set by Seek when a Range restart is triggered.
    /// Governor boosts playback rate until ahead is comfortable or timeout.
    seek_boost: AtomicBool,
    /// Set by the download task after init warmup completes.
    /// Used by handle_play to defer resume-seek until the HTTP stream is stable.
    init_warmup_done: AtomicBool,
    /// Set by handle_seek to pause preload during seeks.
    /// Governor reads this and applies a preload cooldown (no rate boost).
    seek_preload_pause: AtomicBool,
}

impl BufferProgress {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_playback_active(&self, active: bool) {
        self.playback_active.store(active, Relaxed);
    }

    fn take_seek_boost(&self) -> bool {
        self.seek_boost.swap(false, Relaxed)
    }

    /// Request a preload pause during seek (no rate boost, just cooldown).
    pub fn request_seek_preload_pause(&self) {
        self.seek_preload_pause.store(true, Relaxed);
    }

    fn take_seek_preload_pause(&self) -> bool {
        self.seek_preload_pause.swap(false, Relaxed)
    }

    /// Bytes buffered ahead of the decoder read position.
    pub fn ahead(&self) -> u64 {
        self.written
            .load(Relaxed)
            .saturating_sub(self.read_pos.load(Relaxed))
    }

    /// Seconds of audio buffered ahead, or None if bitrate is unknown.
    pub fn ahead_seconds(&self) -> Option<f64> {
        let bps = self.bitrate_bps.load(Relaxed);
        if bps == 0 {
            return None;
        }
        Some(self.ahead() as f64 / bps as f64)
    }

    pub fn reset(&self) {
        self.written.store(0, Relaxed);
        self.read_pos.store(0, Relaxed);
        self.total_len.store(0, Relaxed);
        self.bitrate_bps.store(0, Relaxed);
        self.playback_active.store(false, Relaxed);
        self.seek_boost.store(false, Relaxed);
        self.init_warmup_done.store(false, Relaxed);
        self.seek_preload_pause.store(false, Relaxed);
    }
}

struct TokenRequest {
    class: TrafficClass,
    bytes: u32,
    /// Tokens still owed for this request. Decremented in passes by serve_queue.
    remaining: u32,
    reply: oneshot::Sender<()>,
}

#[derive(Clone)]
pub struct GovernorHandle {
    request_tx: mpsc::Sender<TokenRequest>,
    buffer_progress: Arc<BufferProgress>,
}

impl GovernorHandle {
    /// Request `bytes` worth of bandwidth for `class`.
    /// Blocks (async) until tokens are granted.
    /// If the governor task is dead, returns immediately (ungoverned fallback).
    pub async fn acquire(&self, class: TrafficClass, bytes: u32) {
        let (tx, rx) = oneshot::channel();
        let req = TokenRequest {
            class,
            bytes,
            remaining: bytes,
            reply: tx,
        };
        if self.request_tx.send(req).await.is_err() {
            return; // governor dead — fallback
        }
        let _ = rx.await;
    }

    pub fn buffer_progress(&self) -> &Arc<BufferProgress> {
        &self.buffer_progress
    }

    pub fn reset_buffer_progress(&self) {
        self.buffer_progress.reset();
    }
}

struct TokenBucket {
    tokens: f64,
    burst: f64,
    rate: f64, // bytes/sec
    last_refill: Instant,
}

impl TokenBucket {
    fn new(rate: f64, burst: f64) -> Self {
        Self {
            tokens: burst, // start full
            burst,
            rate,
            last_refill: Instant::now(),
        }
    }

    fn refill(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.last_refill = now;
        self.tokens = (self.tokens + self.rate * elapsed).min(self.burst);
    }

    /// Grants when the full requested amount is available.
    /// Callers must ensure requested <= burst to avoid deadlock.
    fn try_consume(&mut self, requested: u32) -> bool {
        if self.tokens >= requested as f64 {
            self.tokens -= requested as f64;
            true
        } else {
            false
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PreloadGate {
    Active,
    Paused,
}

impl PreloadGate {
    /// Update gate based on buffer state. Returns new gate value.
    fn update(self, bp: &BufferProgress) -> Self {
        let total_len = bp.total_len.load(Relaxed);
        // No active streaming playback → preload always allowed
        if total_len == 0 {
            return PreloadGate::Active;
        }

        // Playback paused/stopped → pause preload to save bandwidth
        if !bp.playback_active.load(Relaxed) {
            return PreloadGate::Paused;
        }

        let ahead = bp.ahead();
        match bp.ahead_seconds() {
            Some(secs) => match self {
                PreloadGate::Active if secs < 1.0 => PreloadGate::Paused,
                PreloadGate::Paused if secs > 2.0 => PreloadGate::Active,
                other => other,
            },
            // Bitrate unknown — fallback byte-based (assume ~200 KB/s)
            None => match self {
                PreloadGate::Active if ahead < 200_000 => PreloadGate::Paused,
                PreloadGate::Paused if ahead > 400_000 => PreloadGate::Active,
                other => other,
            },
        }
    }
}

const TICK_MS: u64 = 25;
const PRELOAD_RATE: f64 = 500_000.0; // 500 KB/s
const PRELOAD_BURST: f64 = 32_000.0; // 32 KB
const METRICS_INTERVAL_TICKS: u32 = (5000 / TICK_MS) as u32; // ~5 seconds

// Adaptive playback rate: multipliers relative to track bitrate (bytes/sec).
// Fallback values used when bitrate is unknown (0).
const PLAYBACK_MULT: f64 = 5.0; // download at 5× real-time
const PLAYBACK_BURST_MULT: f64 = 0.3; // burst covers ~300ms of audio
const FALLBACK_RATE: f64 = 1_500_000.0; // 1.5 MB/s (assumes ~300 KB/s track)
const FALLBACK_BURST: f64 = 48_000.0; // 48 KB

// Buffer-aware playback throttle: avoid downloading the entire file upfront.
// When the buffer is far enough ahead, pause token grants; resume when low.
const PLAYBACK_HIGH_WATER_SECS: f64 = 5.0; // pause download when 5s ahead
const PLAYBACK_LOW_WATER_SECS: f64 = 2.0; // resume when drops to 2s

// Seek boost: multipliers relative to track bitrate
const BOOST_MULT: f64 = 30.0; // 30× real-time after seek
const BOOST_BURST_MULT: f64 = 3.0; // prefill 3s of audio
const BOOST_AHEAD_SECS: f64 = 3.0; // exit boost when 3s ahead
const BOOST_TIMEOUT_MS: u64 = 2000; // exit after 2s max (safeguard)
const MIN_BOOST_AHEAD: u64 = 512 * 1024; // floor: must cover seek padding (512 KB)
const FALLBACK_BOOST_RATE: f64 = 10_000_000.0; // 10 MB/s
const FALLBACK_BOOST_BURST: f64 = 524_288.0; // 512 KB
const FALLBACK_BOOST_AHEAD: u64 = 512 * 1024; // 512 KB

// Starvation watchdog: auto-boost when buffer runs critically low during playback
const STARVATION_AHEAD_SECS: f64 = 1.0; // trigger when less than 1s buffered
const STARVATION_DELAY_MS: u64 = 300; // must persist 300ms before boosting
const STARVATION_AHEAD_FALLBACK: u64 = 128 * 1024; // 128 KB when bitrate unknown

pub fn spawn_governor() -> GovernorHandle {
    let (request_tx, request_rx) = mpsc::channel::<TokenRequest>(64);
    let buffer_progress = Arc::new(BufferProgress::new());

    let bp = buffer_progress.clone();
    tokio::spawn(governor_loop(request_rx, bp));

    GovernorHandle {
        request_tx,
        buffer_progress,
    }
}

struct GovernorState {
    playback_bucket: TokenBucket,
    preload_bucket: TokenBucket,
    playback_queue: VecDeque<TokenRequest>,
    preload_queue: VecDeque<TokenRequest>,
    gate: PreloadGate,
    boost_start: Option<Instant>,
    preload_cooldown_until: Option<Instant>,
    saved_rate: f64,
    saved_burst: f64,
    last_bitrate: u64,
    play_bytes: u64,
    preload_bytes: u64,
    pause_time: std::time::Duration,
    pause_start: Option<Instant>,
    tick_count: u32,
    starvation_since: Option<Instant>,
    playback_throttled: bool,
}

impl GovernorState {
    fn new() -> Self {
        let (rate, burst) = playback_params(0);
        Self {
            playback_bucket: TokenBucket::new(rate, burst),
            preload_bucket: TokenBucket::new(PRELOAD_RATE, PRELOAD_BURST),
            playback_queue: VecDeque::new(),
            preload_queue: VecDeque::new(),
            gate: PreloadGate::Active,
            boost_start: None,
            preload_cooldown_until: None,
            saved_rate: rate,
            saved_burst: burst,
            last_bitrate: 0,
            play_bytes: 0,
            preload_bytes: 0,
            pause_time: std::time::Duration::ZERO,
            pause_start: None,
            tick_count: 0,
            starvation_since: None,
            playback_throttled: false,
        }
    }

    fn tick(&mut self, bp: &BufferProgress) {
        self.playback_bucket.refill();
        self.preload_bucket.refill();
        self.update_adaptive_rate(bp);
        self.update_boost(bp);
        self.update_starvation_watchdog(bp);
        self.update_playback_throttle(bp);
        // Pause preload during seeks (no playback rate boost, just cooldown)
        if bp.take_seek_preload_pause() {
            self.preload_cooldown_until =
                Some(Instant::now() + std::time::Duration::from_millis(1500));
            crate::vprintln!("[GOV]    Preload paused for seek (1.5s cooldown)");
        }
        self.update_gate(bp);
        if !self.playback_throttled {
            serve_queue(
                &mut self.playback_queue,
                &mut self.playback_bucket,
                &mut self.play_bytes,
            );
        }
        // Clear cooldown when no track is active (stop/load reset)
        if bp.total_len.load(Relaxed) == 0 {
            self.preload_cooldown_until = None;
        }
        let cooldown_active = self
            .preload_cooldown_until
            .is_some_and(|t| Instant::now() < t);
        if self.gate == PreloadGate::Active && self.boost_start.is_none() && !cooldown_active {
            serve_queue(
                &mut self.preload_queue,
                &mut self.preload_bucket,
                &mut self.preload_bytes,
            );
        }
        self.emit_metrics(bp);
    }

    fn update_adaptive_rate(&mut self, bp: &BufferProgress) {
        let bitrate = bp.bitrate_bps.load(Relaxed);
        if bitrate == self.last_bitrate || self.boost_start.is_some() {
            return;
        }
        let (new_rate, new_burst) = playback_params(bitrate);
        self.playback_bucket.rate = new_rate;
        self.playback_bucket.burst = new_burst;
        self.saved_rate = new_rate;
        self.saved_burst = new_burst;
        if bitrate > 0 {
            crate::vprintln!(
                "[GOV]    Adaptive rate: {:.0} KB/s (bitrate: {:.0} KB/s, {:.0}× real-time)",
                new_rate / 1024.0,
                bitrate as f64 / 1024.0,
                PLAYBACK_MULT
            );
        } else {
            crate::vprintln!(
                "[GOV]    Adaptive rate: reset to fallback ({:.0} KB/s)",
                new_rate / 1024.0
            );
        }
        self.last_bitrate = bitrate;
    }

    fn update_boost(&mut self, bp: &BufferProgress) {
        let bitrate = bp.bitrate_bps.load(Relaxed);
        let (boost_rate, boost_burst, boost_ahead_threshold) = boost_params(bitrate);

        if bp.take_seek_boost() {
            if self.boost_start.is_none() {
                self.saved_rate = self.playback_bucket.rate;
                self.saved_burst = self.playback_bucket.burst;
            }
            self.playback_bucket.rate = boost_rate;
            self.playback_bucket.burst = boost_burst;
            self.playback_bucket.tokens = boost_burst;
            self.boost_start = Some(Instant::now());
            self.preload_cooldown_until =
                Some(Instant::now() + std::time::Duration::from_millis(1500));
            crate::vprintln!(
                "[GOV]    Seek BOOST ON (rate: {:.0} KB/s, {:.0}× real-time)",
                boost_rate / 1024.0,
                if bitrate > 0 { BOOST_MULT } else { 0.0 }
            );
        } else if let Some(start) = self.boost_start {
            let ahead = bp.ahead();
            let elapsed_ms = start.elapsed().as_millis() as u64;

            if ahead >= boost_ahead_threshold {
                self.exit_boost();
                crate::vprintln!(
                    "[GOV]    Seek BOOST OFF (threshold: {} KB ahead, {elapsed_ms}ms)",
                    ahead / 1024
                );
            } else if elapsed_ms >= BOOST_TIMEOUT_MS {
                self.exit_boost();
                crate::vprintln!(
                    "[GOV]    Seek BOOST OFF (timeout {elapsed_ms}ms, {} KB ahead)",
                    ahead / 1024
                );
            }
        }
    }

    fn update_starvation_watchdog(&mut self, bp: &BufferProgress) {
        // Skip if already boosting (seek boost or previous starvation boost)
        if self.boost_start.is_some() {
            self.starvation_since = None;
            return;
        }

        // Only detect during active playback with an active stream
        if !bp.playback_active.load(Relaxed) {
            self.starvation_since = None;
            return;
        }
        let total_len = bp.total_len.load(Relaxed);
        if total_len == 0 {
            self.starvation_since = None;
            return;
        }

        // Download complete — no starvation possible
        let written = bp.written.load(Relaxed);
        if written >= total_len {
            self.starvation_since = None;
            return;
        }

        let ahead = bp.ahead();
        let bitrate = bp.bitrate_bps.load(Relaxed);
        let starving = if bitrate > 0 {
            (ahead as f64) < bitrate as f64 * STARVATION_AHEAD_SECS
        } else {
            ahead < STARVATION_AHEAD_FALLBACK
        };

        if starving {
            let since = self.starvation_since.get_or_insert(Instant::now());
            if since.elapsed().as_millis() as u64 >= STARVATION_DELAY_MS {
                // Enter starvation boost through the same mechanism as seek boost
                self.saved_rate = self.playback_bucket.rate;
                self.saved_burst = self.playback_bucket.burst;
                let (boost_rate, boost_burst, _) = boost_params(bitrate);
                self.playback_bucket.rate = boost_rate;
                self.playback_bucket.burst = boost_burst;
                self.playback_bucket.tokens = boost_burst;
                self.boost_start = Some(Instant::now());
                self.starvation_since = None;
                // No preload cooldown for starvation boost (unlike seek boost)
                let ahead_secs = if bitrate > 0 {
                    ahead as f64 / bitrate as f64
                } else {
                    0.0
                };
                crate::vprintln!(
                    "[GOV]    Starvation BOOST ON (buffer: {} KB, {:.1}s ahead)",
                    ahead / 1024,
                    ahead_secs
                );
            }
        } else {
            self.starvation_since = None;
        }
    }

    fn update_playback_throttle(&mut self, bp: &BufferProgress) {
        // Don't throttle during seek boost — data is needed urgently
        if self.boost_start.is_some() {
            if self.playback_throttled {
                self.playback_throttled = false;
            }
            return;
        }
        // Don't throttle when no active stream (stopped/loading)
        if bp.total_len.load(Relaxed) == 0 {
            self.playback_throttled = false;
            return;
        }
        // Don't throttle if download is complete
        let written = bp.written.load(Relaxed);
        if written >= bp.total_len.load(Relaxed) {
            self.playback_throttled = false;
            return;
        }

        match bp.ahead_seconds() {
            Some(secs) => {
                if !self.playback_throttled && secs >= PLAYBACK_HIGH_WATER_SECS {
                    self.playback_throttled = true;
                    crate::vprintln!("[GOV]    Playback throttled (buffer: {secs:.1}s ahead)");
                } else if self.playback_throttled && secs < PLAYBACK_LOW_WATER_SECS {
                    self.playback_throttled = false;
                    crate::vprintln!("[GOV]    Playback resumed (buffer: {secs:.1}s ahead)");
                }
            }
            None => {
                // Bitrate unknown — fallback byte-based
                let ahead = bp.ahead();
                if !self.playback_throttled && ahead > 1_000_000 {
                    self.playback_throttled = true;
                } else if self.playback_throttled && ahead < 400_000 {
                    self.playback_throttled = false;
                }
            }
        }
    }

    fn exit_boost(&mut self) {
        self.playback_bucket.rate = self.saved_rate;
        self.playback_bucket.burst = self.saved_burst;
        self.boost_start = None;
    }

    fn update_gate(&mut self, bp: &BufferProgress) {
        let new_gate = self.gate.update(bp);
        if new_gate != self.gate {
            match new_gate {
                PreloadGate::Paused => {
                    crate::vprintln!("[GOV]    Preload PAUSED (buffer low)");
                    self.pause_start = Some(Instant::now());
                }
                PreloadGate::Active => {
                    crate::vprintln!("[GOV]    Preload RESUMED (buffer ok)");
                    if let Some(start) = self.pause_start.take() {
                        self.pause_time += start.elapsed();
                    }
                }
            }
            self.gate = new_gate;
        }
    }

    fn emit_metrics(&mut self, bp: &BufferProgress) {
        self.tick_count += 1;
        if self.tick_count >= METRICS_INTERVAL_TICKS {
            let period = (self.tick_count as f64 * TICK_MS as f64) / 1000.0;
            let play_rate = self.play_bytes as f64 / period / 1024.0;
            let preload_rate = self.preload_bytes as f64 / period / 1024.0;
            let ahead_kb = bp.ahead() / 1024;
            let secs_str = bp
                .ahead_seconds()
                .map_or("?s".into(), |s| format!("{s:.1}s"));
            let current_pause = if self.gate == PreloadGate::Paused {
                self.pause_start.map(|s| s.elapsed()).unwrap_or_default() + self.pause_time
            } else {
                self.pause_time
            };
            let boost_str = if self.boost_start.is_some() {
                " [BOOST]"
            } else {
                ""
            };
            if self.boost_start.is_some() || play_rate > 0.0 || preload_rate > 0.0 {
                crate::vprintln!(
                    "[GOV]    play: {:.0} KB/s | preload: {:.0} KB/s | buf: {} KB ({}) | preload: {:?} | pause: {:.1}s{}",
                    play_rate,
                    preload_rate,
                    ahead_kb,
                    secs_str,
                    self.gate,
                    current_pause.as_secs_f64(),
                    boost_str
                );
            }

            self.play_bytes = 0;
            self.preload_bytes = 0;
            self.pause_time = std::time::Duration::ZERO;
            if self.gate == PreloadGate::Paused {
                self.pause_start = Some(Instant::now());
            }
            self.tick_count = 0;
        }
    }
}

fn playback_params(bitrate: u64) -> (f64, f64) {
    if bitrate > 0 {
        (
            bitrate as f64 * PLAYBACK_MULT,
            bitrate as f64 * PLAYBACK_BURST_MULT,
        )
    } else {
        (FALLBACK_RATE, FALLBACK_BURST)
    }
}

fn boost_params(bitrate: u64) -> (f64, f64, u64) {
    if bitrate > 0 {
        (
            bitrate as f64 * BOOST_MULT,
            bitrate as f64 * BOOST_BURST_MULT,
            ((bitrate as f64 * BOOST_AHEAD_SECS) as u64).max(MIN_BOOST_AHEAD),
        )
    } else {
        (
            FALLBACK_BOOST_RATE,
            FALLBACK_BOOST_BURST,
            FALLBACK_BOOST_AHEAD,
        )
    }
}

async fn governor_loop(mut rx: mpsc::Receiver<TokenRequest>, bp: Arc<BufferProgress>) {
    let mut state = GovernorState::new();
    let mut interval = time::interval(std::time::Duration::from_millis(TICK_MS));
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            biased;
            _ = interval.tick() => state.tick(&bp),
            req = rx.recv() => match req {
                Some(r) => {
                    // Serve immediately on arrival instead of waiting for the next
                    // tick. Without this, max throughput is ~640 KB/s (16KB / 25ms)
                    // which is below the 5× target for high-bitrate tracks.
                    match r.class {
                        TrafficClass::Playback => {
                            state.playback_queue.push_back(r);
                            if !state.playback_throttled {
                                state.playback_bucket.refill();
                                serve_queue(
                                    &mut state.playback_queue,
                                    &mut state.playback_bucket,
                                    &mut state.play_bytes,
                                );
                            }
                        }
                        TrafficClass::Preload => {
                            state.preload_queue.push_back(r);
                            let cooldown_active = state
                                .preload_cooldown_until
                                .is_some_and(|t| Instant::now() < t);
                            if state.gate == PreloadGate::Active
                                && state.boost_start.is_none()
                                && !cooldown_active
                            {
                                state.preload_bucket.refill();
                                serve_queue(
                                    &mut state.preload_queue,
                                    &mut state.preload_bucket,
                                    &mut state.preload_bytes,
                                );
                            }
                        }
                    }
                }
                None => {
                    drain_queue_ungoverned(&mut state.playback_queue);
                    drain_queue_ungoverned(&mut state.preload_queue);
                    return;
                }
            }
        }
    }
}

/// Try to grant tokens to queued requests.
/// Oversized chunks (> burst) are consumed in multiple passes across ticks.
fn serve_queue(queue: &mut VecDeque<TokenRequest>, bucket: &mut TokenBucket, bytes_acc: &mut u64) {
    while let Some(front) = queue.front_mut() {
        let bite = (front.remaining as f64).min(bucket.burst) as u32;
        if bucket.try_consume(bite) {
            front.remaining -= bite;
            if front.remaining == 0 {
                let req = queue.pop_front().unwrap();
                *bytes_acc += req.bytes as u64;
                let _ = req.reply.send(());
            }
        } else {
            break; // Not enough tokens this tick
        }
    }
}

/// Drain a queue by granting full requested amounts (ungoverned fallback).
fn drain_queue_ungoverned(queue: &mut VecDeque<TokenRequest>) {
    while let Some(req) = queue.pop_front() {
        let _ = req.reply.send(());
    }
}
