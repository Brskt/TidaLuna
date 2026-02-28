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
/// The governor reads these on every tick (15 ms) to compute hysteresis.
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
}

impl BufferProgress {
    pub fn new() -> Self {
        Self {
            written: AtomicU64::new(0),
            read_pos: AtomicU64::new(0),
            total_len: AtomicU64::new(0),
            bitrate_bps: AtomicU64::new(0),
            playback_active: AtomicBool::new(false),
            seek_boost: AtomicBool::new(false),
        }
    }

    pub fn set_playback_active(&self, active: bool) {
        self.playback_active.store(active, Relaxed);
    }

    pub fn request_seek_boost(&self) {
        self.seek_boost.store(true, Relaxed);
    }

    fn take_seek_boost(&self) -> bool {
        self.seek_boost.swap(false, Relaxed)
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
                PreloadGate::Active if secs < 8.0 => PreloadGate::Paused,
                PreloadGate::Paused if secs > 20.0 => PreloadGate::Active,
                other => other,
            },
            // Bitrate unknown — fallback byte-based (assume ~200 KB/s)
            None => match self {
                PreloadGate::Active if ahead < 1_600_000 => PreloadGate::Paused,
                PreloadGate::Paused if ahead > 4_000_000 => PreloadGate::Active,
                other => other,
            },
        }
    }
}

const TICK_MS: u64 = 15;
const PRELOAD_RATE: f64 = 500_000.0; // 500 KB/s
const PRELOAD_BURST: f64 = 32_000.0; // 32 KB
const METRICS_INTERVAL_TICKS: u32 = (5000 / TICK_MS) as u32; // ~5 seconds

// Adaptive playback rate: multipliers relative to track bitrate (bytes/sec).
// Fallback values used when bitrate is unknown (0).
const PLAYBACK_MULT: f64 = 5.0; // download at 5× real-time
const PLAYBACK_BURST_MULT: f64 = 0.3; // burst covers ~300ms of audio
const FALLBACK_RATE: f64 = 1_500_000.0; // 1.5 MB/s (assumes ~300 KB/s track)
const FALLBACK_BURST: f64 = 48_000.0; // 48 KB

// Seek boost: multipliers relative to track bitrate
const BOOST_MULT: f64 = 30.0; // 30× real-time after seek
const BOOST_BURST_MULT: f64 = 3.0; // prefill 3s of audio
const BOOST_AHEAD_SECS: f64 = 3.0; // exit boost when 3s ahead
const BOOST_TIMEOUT_MS: u64 = 2000; // exit after 2s max (safeguard)
const MIN_BOOST_AHEAD: u64 = 512 * 1024; // floor: must cover seek padding (512 KB)
const FALLBACK_BOOST_RATE: f64 = 10_000_000.0; // 10 MB/s
const FALLBACK_BOOST_BURST: f64 = 524_288.0; // 512 KB
const FALLBACK_BOOST_AHEAD: u64 = 512 * 1024; // 512 KB

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
        }
    }

    fn tick(&mut self, bp: &BufferProgress) {
        self.playback_bucket.refill();
        self.preload_bucket.refill();
        self.update_adaptive_rate(bp);
        self.update_boost(bp);
        self.update_gate(bp);
        serve_queue(
            &mut self.playback_queue,
            &mut self.playback_bucket,
            &mut self.play_bytes,
        );
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
            eprintln!(
                "[GOV]    Adaptive rate: {:.0} KB/s (bitrate: {:.0} KB/s, {:.0}× real-time)",
                new_rate / 1024.0,
                bitrate as f64 / 1024.0,
                PLAYBACK_MULT
            );
        } else {
            eprintln!(
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
            eprintln!(
                "[GOV]    Seek BOOST ON (rate: {:.0} KB/s, {:.0}× real-time)",
                boost_rate / 1024.0,
                if bitrate > 0 { BOOST_MULT } else { 0.0 }
            );
        } else if let Some(start) = self.boost_start {
            let ahead = bp.ahead();
            let elapsed_ms = start.elapsed().as_millis() as u64;

            if ahead >= boost_ahead_threshold {
                self.exit_boost();
                eprintln!(
                    "[GOV]    Seek BOOST OFF (threshold: {} KB ahead, {elapsed_ms}ms)",
                    ahead / 1024
                );
            } else if elapsed_ms >= BOOST_TIMEOUT_MS {
                self.exit_boost();
                eprintln!(
                    "[GOV]    Seek BOOST OFF (timeout {elapsed_ms}ms, {} KB ahead)",
                    ahead / 1024
                );
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
                    eprintln!("[GOV]    Preload PAUSED (buffer low)");
                    self.pause_start = Some(Instant::now());
                }
                PreloadGate::Active => {
                    eprintln!("[GOV]    Preload RESUMED (buffer ok)");
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
            eprintln!(
                "[GOV]    play: {:.0} KB/s | preload: {:.0} KB/s | buf: {} KB ({}) | gate: {:?} | pause: {:.1}s{}",
                play_rate,
                preload_rate,
                ahead_kb,
                secs_str,
                self.gate,
                current_pause.as_secs_f64(),
                boost_str
            );

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
                Some(r) => match r.class {
                    TrafficClass::Playback => state.playback_queue.push_back(r),
                    TrafficClass::Preload => state.preload_queue.push_back(r),
                },
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
