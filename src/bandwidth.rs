use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::time::Instant;
use tokio::sync::{mpsc, oneshot};
use tokio::time::{self, MissedTickBehavior};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Internal protocol
// ---------------------------------------------------------------------------

struct TokenRequest {
    class: TrafficClass,
    bytes: u32,
    reply: oneshot::Sender<u32>,
}

// ---------------------------------------------------------------------------
// GovernorHandle — public API
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct GovernorHandle {
    request_tx: mpsc::Sender<TokenRequest>,
    buffer_progress: Arc<BufferProgress>,
}

impl GovernorHandle {
    /// Request `bytes` worth of bandwidth for `class`.
    /// Blocks (async) until at least some tokens are granted.
    /// Returns the number of bytes actually granted.
    /// If the governor task is dead, returns `bytes` (ungoverned fallback).
    pub async fn acquire(&self, class: TrafficClass, bytes: u32) -> u32 {
        let (tx, rx) = oneshot::channel();
        let req = TokenRequest {
            class,
            bytes,
            reply: tx,
        };
        if self.request_tx.send(req).await.is_err() {
            return bytes; // governor dead — fallback
        }
        rx.await.unwrap_or(bytes)
    }

    pub fn buffer_progress(&self) -> &Arc<BufferProgress> {
        &self.buffer_progress
    }

    pub fn reset_buffer_progress(&self) {
        self.buffer_progress.reset();
    }
}

// ---------------------------------------------------------------------------
// TokenBucket
// ---------------------------------------------------------------------------

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

    /// Only grants when the full requested amount is available.
    /// Prevents partial grants that would let oversized chunks through at ~2x rate.
    fn try_consume(&mut self, requested: u32) -> bool {
        if self.tokens >= requested as f64 {
            self.tokens -= requested as f64;
            true
        } else {
            false
        }
    }
}

// ---------------------------------------------------------------------------
// Hysteresis gate
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Governor task
// ---------------------------------------------------------------------------

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

async fn governor_loop(mut request_rx: mpsc::Receiver<TokenRequest>, bp: Arc<BufferProgress>) {
    let mut playback_bucket = TokenBucket::new(FALLBACK_RATE, FALLBACK_BURST);
    let mut preload_bucket = TokenBucket::new(PRELOAD_RATE, PRELOAD_BURST);

    let mut playback_queue: VecDeque<TokenRequest> = VecDeque::new();
    let mut preload_queue: VecDeque<TokenRequest> = VecDeque::new();

    let mut gate = PreloadGate::Active;

    // Seek boost state
    let mut boost_start: Option<Instant> = None;
    let mut saved_rate: f64 = FALLBACK_RATE;
    let mut saved_burst: f64 = FALLBACK_BURST;
    let mut last_bitrate: u64 = 0;

    // Metrics accumulators
    let mut play_bytes: u64 = 0;
    let mut preload_bytes: u64 = 0;
    let mut pause_time = std::time::Duration::ZERO;
    let mut pause_start: Option<Instant> = None;
    let mut tick_count: u32 = 0;

    let mut interval = time::interval(std::time::Duration::from_millis(TICK_MS));
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            biased;

            _ = interval.tick() => {
                // 1. Refill buckets
                playback_bucket.refill();
                preload_bucket.refill();

                // 2. Adapt playback rate to track bitrate
                let bitrate = bp.bitrate_bps.load(Relaxed);
                if bitrate != last_bitrate && bitrate > 0 && boost_start.is_none() {
                    let new_rate = bitrate as f64 * PLAYBACK_MULT;
                    let new_burst = bitrate as f64 * PLAYBACK_BURST_MULT;
                    playback_bucket.rate = new_rate;
                    playback_bucket.burst = new_burst;
                    saved_rate = new_rate;
                    saved_burst = new_burst;
                    eprintln!(
                        "[GOV]    Adaptive rate: {:.0} KB/s (bitrate: {:.0} KB/s, {:.0}× real-time)",
                        new_rate / 1024.0,
                        bitrate as f64 / 1024.0,
                        PLAYBACK_MULT
                    );
                    last_bitrate = bitrate;
                }

                // 3. Seek boost management
                let (boost_rate, boost_burst, boost_ahead_threshold) = if bitrate > 0 {
                    (
                        bitrate as f64 * BOOST_MULT,
                        bitrate as f64 * BOOST_BURST_MULT,
                        (bitrate as f64 * BOOST_AHEAD_SECS) as u64,
                    )
                } else {
                    (FALLBACK_BOOST_RATE, FALLBACK_BOOST_BURST, FALLBACK_BOOST_AHEAD)
                };

                if bp.seek_boost.swap(false, Relaxed) {
                    // Enter boost — save current rate/burst, apply boosted values
                    if boost_start.is_none() {
                        saved_rate = playback_bucket.rate;
                        saved_burst = playback_bucket.burst;
                    }
                    playback_bucket.rate = boost_rate;
                    playback_bucket.burst = boost_burst;
                    playback_bucket.tokens = boost_burst; // prefill for immediate serving
                    boost_start = Some(Instant::now());
                    eprintln!("[GOV]    Seek BOOST ON (rate: {:.0} KB/s, {:.0}× real-time)",
                        boost_rate / 1024.0, if bitrate > 0 { BOOST_MULT } else { 0.0 });
                } else if let Some(start) = boost_start {
                    let ahead = bp.ahead();
                    let elapsed_ms = start.elapsed().as_millis() as u64;

                    if ahead >= boost_ahead_threshold {
                        // Buffer has enough ahead — exit boost
                        playback_bucket.rate = saved_rate;
                        playback_bucket.burst = saved_burst;
                        boost_start = None;
                        eprintln!("[GOV]    Seek BOOST OFF (threshold: {} KB ahead, {elapsed_ms}ms)",
                            ahead / 1024);
                    } else if elapsed_ms >= BOOST_TIMEOUT_MS {
                        // Timeout safeguard — exit boost
                        playback_bucket.rate = saved_rate;
                        playback_bucket.burst = saved_burst;
                        boost_start = None;
                        eprintln!("[GOV]    Seek BOOST OFF (timeout {elapsed_ms}ms, {} KB ahead)",
                            ahead / 1024);
                    }
                }

                // 4. Update hysteresis gate
                let new_gate = gate.update(&bp);
                if new_gate != gate {
                    match new_gate {
                        PreloadGate::Paused => {
                            eprintln!("[GOV]    Preload PAUSED (buffer low)");
                            pause_start = Some(Instant::now());
                        }
                        PreloadGate::Active => {
                            eprintln!("[GOV]    Preload RESUMED (buffer ok)");
                            if let Some(start) = pause_start.take() {
                                pause_time += start.elapsed();
                            }
                        }
                    }
                    gate = new_gate;
                }

                // 5. Serve playback queue (priority)
                serve_queue(&mut playback_queue, &mut playback_bucket, &mut play_bytes);

                // 6. Serve preload queue (only if gate active AND not boosting)
                if gate == PreloadGate::Active && boost_start.is_none() {
                    serve_queue(&mut preload_queue, &mut preload_bucket, &mut preload_bytes);
                }

                // 7. Periodic metrics
                tick_count += 1;
                if tick_count >= METRICS_INTERVAL_TICKS {
                    let period = (tick_count as f64 * TICK_MS as f64) / 1000.0;
                    let play_rate = play_bytes as f64 / period / 1024.0;
                    let preload_rate = preload_bytes as f64 / period / 1024.0;
                    let ahead_kb = bp.ahead() / 1024;
                    let secs_str = match bp.ahead_seconds() {
                        Some(s) => format!("{:.1}s", s),
                        None => "?s".to_string(),
                    };
                    let current_pause = if gate == PreloadGate::Paused {
                        pause_start.map(|s| s.elapsed()).unwrap_or_default() + pause_time
                    } else {
                        pause_time
                    };
                    let boost_str = if boost_start.is_some() { " [BOOST]" } else { "" };
                    eprintln!(
                        "[GOV]    play: {:.0} KB/s | preload: {:.0} KB/s | buf: {} KB ({}) | gate: {:?} | pause: {:.1}s{}",
                        play_rate, preload_rate, ahead_kb, secs_str, gate, current_pause.as_secs_f64(), boost_str
                    );

                    // Reset accumulators
                    play_bytes = 0;
                    preload_bytes = 0;
                    pause_time = std::time::Duration::ZERO;
                    if gate == PreloadGate::Paused {
                        pause_start = Some(Instant::now());
                    }
                    tick_count = 0;
                }
            }

            req = request_rx.recv() => {
                match req {
                    Some(r) => {
                        match r.class {
                            TrafficClass::Playback => playback_queue.push_back(r),
                            TrafficClass::Preload => preload_queue.push_back(r),
                        }
                    }
                    None => {
                        // All senders dropped — drain queues and exit
                        drain_queue_ungoverned(&mut playback_queue);
                        drain_queue_ungoverned(&mut preload_queue);
                        return;
                    }
                }
            }
        }
    }
}

/// Try to grant tokens to queued requests. Removes fulfilled requests.
fn serve_queue(queue: &mut VecDeque<TokenRequest>, bucket: &mut TokenBucket, bytes_acc: &mut u64) {
    while let Some(front) = queue.front() {
        if bucket.try_consume(front.bytes) {
            let req = queue.pop_front().unwrap();
            *bytes_acc += req.bytes as u64;
            let _ = req.reply.send(req.bytes);
        } else {
            break; // Not enough tokens this tick
        }
    }
}

/// Drain a queue by granting full requested amounts (ungoverned fallback).
fn drain_queue_ungoverned(queue: &mut VecDeque<TokenRequest>) {
    while let Some(req) = queue.pop_front() {
        let _ = req.reply.send(req.bytes);
    }
}
