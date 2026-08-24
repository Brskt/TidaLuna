use std::io::{self, Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use symphonia::core::io::MediaSource;

const INITIAL_BUFFER_CAP: usize = 2 * 1024 * 1024; // 2 MB

/// When the read cursor is slightly ahead of buf_end, wait for the sequential
/// download to catch up instead of triggering a new Range restart. At the
/// governed download rate this lookahead arrives in well under a restart's
/// TTFB; keep it small enough that waiting never loses to restarting.
const SEEK_LOOKAHEAD: u64 = 32 * 1024; // 32 KB

struct Inner {
    data: Vec<u8>,
    base_offset: u64,
    total_len: u64,
    finished: bool,
    cancelled: bool,
    error: Option<String>,
    restart_target: Option<u64>,
    /// Ciphertext staging file for the disk cache, with its byte count. The download
    /// loop owns it while streaming and parks it here only at EOF: no per-chunk
    /// write ever takes this lock.
    ciphertext: Option<(tempfile::NamedTempFile, u64)>,
}

/// Shared state between readers, writers, and the async download loop.
struct SharedState {
    inner: Mutex<Inner>,
    cvar: Condvar,
    /// Bytes written (absolute file offset = base_offset + data.len()).
    written: AtomicU64,
    cancelled_atomic: AtomicBool,
    /// Last cursor position after a successful read (for governor buffer tracking).
    read_cursor: AtomicU64,
    /// Wake the async download loop on restart/cancel.
    async_notify: tokio::sync::Notify,
    /// True while the read side is blocked waiting for data.
    stalled: AtomicBool,
}

/// A RAM buffer that supports streaming writes and blocking reads.
///
/// Designed to be the sole data source for symphonia's `MediaSourceStream`.
/// Implements `Read + Seek + MediaSource`.
///
/// Two access patterns:
/// 1. **Streaming**: Download task writes chunks via `RamBufferWriter`.
///    Reader blocks if data is not yet available. Forward seek past buffer
///    triggers a Range restart (reset buffer, new base_offset).
/// 2. **Complete**: All data loaded upfront (cache hit or preload).
///    All seeks are instant.
#[derive(Clone)]
pub struct RamBuffer {
    shared: Arc<SharedState>,
    cursor: u64, // reader's current position (absolute file offset)
    // Per-reader stop: `read` returns Interrupted without touching the shared
    // `cancelled` (which retires every reader). Drops a stale exclusive reader on
    // a mode switch, stopping it from fighting the new shared reader.
    reader_cancel: Option<Arc<AtomicBool>>,
}

/// Write-side handle for the async download task.
pub struct RamBufferWriter {
    shared: Arc<SharedState>,
}

impl RamBuffer {
    pub fn new(total_len: u64) -> (Self, RamBufferWriter) {
        let shared = Arc::new(SharedState {
            inner: Mutex::new(Inner {
                data: Vec::with_capacity((total_len.min(INITIAL_BUFFER_CAP as u64)) as usize),
                base_offset: 0,
                total_len,
                finished: false,
                cancelled: false,
                error: None,
                restart_target: None,
                ciphertext: None,
            }),
            cvar: Condvar::new(),
            written: AtomicU64::new(0),
            cancelled_atomic: AtomicBool::new(false),
            read_cursor: AtomicU64::new(0),
            async_notify: tokio::sync::Notify::new(),
            stalled: AtomicBool::new(false),
        });

        let buffer = RamBuffer {
            shared: shared.clone(),
            cursor: 0,
            reader_cancel: None,
        };
        let writer = RamBufferWriter { shared };
        (buffer, writer)
    }

    pub fn from_complete(data: Vec<u8>) -> Self {
        Self::from_complete_with_ciphertext(data, None)
    }

    /// A complete buffer that also carries the ciphertext for the disk cache:
    /// a preload hit or a no-Content-Length download can still be stored.
    pub fn from_complete_with_ciphertext(
        data: Vec<u8>,
        ciphertext: Option<(tempfile::NamedTempFile, u64)>,
    ) -> Self {
        let total_len = data.len() as u64;
        let shared = Arc::new(SharedState {
            inner: Mutex::new(Inner {
                data,
                base_offset: 0,
                total_len,
                finished: true,
                cancelled: false,
                error: None,
                restart_target: None,
                ciphertext,
            }),
            cvar: Condvar::new(),
            written: AtomicU64::new(total_len),
            cancelled_atomic: AtomicBool::new(false),
            read_cursor: AtomicU64::new(0),
            async_notify: tokio::sync::Notify::new(),
            stalled: AtomicBool::new(false),
        });

        RamBuffer {
            shared,
            cursor: 0,
            reader_cancel: None,
        }
    }

    /// Attach a per-reader stop signal: when set, this reader's `read` returns
    /// Interrupted, leaving other readers untouched. This is what lets a buffer outlive
    /// the decoder that was reading it, for the next decoder to pick up.
    pub fn with_reader_cancel(mut self, cancel: Arc<AtomicBool>) -> Self {
        self.reader_cancel = Some(cancel);
        self
    }

    /// Wake any reader blocked in `read` to re-check its stop signal. Setting the signal
    /// without this leaves the reader parked until its own timeout expires.
    pub fn wake_readers(&self) {
        self.shared.cvar.notify_all();
    }

    pub fn cancel(&self) {
        let mut inner = self.shared.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.cancelled = true;
        self.shared.cancelled_atomic.store(true, Relaxed);
        self.shared.cvar.notify_all();
        self.shared.async_notify.notify_one();
    }

    /// Returns true if the entire file has been downloaded without error.
    pub fn is_complete(&self) -> bool {
        let inner = self.shared.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.finished && inner.error.is_none() && inner.base_offset == 0
    }

    /// Like `is_complete()` but also requires the data to still be in memory. A
    /// reused decoder needs the bytes present; a complete but empty buffer would
    /// read instant EOF (silence). A `true` here guarantees a fresh decoder can
    /// read the track.
    // Callers are all cfg(windows) (the buffer-reuse gates in thread/device.rs), and
    // Linux clippy therefore sees it as dead; keep it compiled/type-checked there anyway
    // (same pattern as declick.rs / convert.rs).
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub fn is_reusable(&self) -> bool {
        let inner = self.shared.inner.lock().unwrap_or_else(|e| e.into_inner());
        // A cancel outlives the download it stopped and there is no un-cancel: every later
        // read reports Interrupted; a finished buffer that was cancelled reads no better
        // than an empty one.
        !inner.cancelled
            && inner.finished
            && inner.error.is_none()
            && inner.base_offset == 0
            && !inner.data.is_empty()
    }

    pub fn total_len(&self) -> u64 {
        let inner = self.shared.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.total_len
    }

    /// Take the ciphertext staging file the download parked at EOF. Gated on the same
    /// completeness as `is_complete()`: a partial or Range-restarted download never
    /// yields one. No size check; `CipherSink::finish` refuses an empty sink, and none
    /// ever reaches the buffer.
    pub fn take_ciphertext(&self) -> Option<(tempfile::NamedTempFile, u64)> {
        let mut inner = self.shared.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.finished && inner.error.is_none() && inner.base_offset == 0 {
            inner.ciphertext.take()
        } else {
            None
        }
    }

    pub fn written(&self) -> u64 {
        self.shared.written.load(Relaxed)
    }

    /// Current read cursor position (absolute byte offset).
    /// Updated after each successful read by the decode thread.
    pub fn read_cursor(&self) -> u64 {
        self.shared.read_cursor.load(Relaxed)
    }

    /// True when the decode thread is blocked waiting for data.
    pub fn is_stalled(&self) -> bool {
        self.shared.stalled.load(Relaxed)
    }

    /// Wait until the writer appends data, finishes, or an error occurs.
    pub async fn notified(&self) {
        self.shared.async_notify.notified().await;
    }
}

impl Read for RamBuffer {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        // Liveness is judged here rather than in the download task, because this is the only
        // side that knows anyone is HARMED. The writer goes quiet for reasons that are entirely
        // correct (parked after a partial EOF, or holding back while the governor withholds
        // playback tokens), and a clock on the writer's own idleness cannot tell those from a
        // dead server. Counted locally: a starved reader never leaves this call, so the count
        // is per-wait by construction and a reader that gets even one byte starts over.
        //
        // Six cycles, matching the tolerance the product already shipped. The floor it has to
        // clear is the download's own retry budget: eight reconnects backing off 250ms x attempt
        // sums to 9s of sleep before the writer gives up and reports the failure itself.
        const STALL_CYCLES: u32 = 6;
        let mut starved_cycles: u32 = 0;

        let mut inner = self.shared.inner.lock().unwrap_or_else(|e| e.into_inner());

        loop {
            if inner.cancelled {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "streaming cancelled",
                ));
            }

            // Retired reader (e.g. a stale exclusive stream after a mode switch):
            // bail without disturbing the shared state the live reader depends on.
            if let Some(ref c) = self.reader_cancel
                && c.load(Relaxed)
            {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "reader retired"));
            }

            if let Some(ref err) = inner.error {
                return Err(io::Error::other(err.clone()));
            }

            let buf_start = inner.base_offset;
            let buf_end = inner.base_offset + inner.data.len() as u64;

            if self.cursor >= buf_start && self.cursor < buf_end {
                let start = (self.cursor - buf_start) as usize;
                let end = std::cmp::min(start + buf.len(), inner.data.len());
                let n = end - start;
                buf[..n].copy_from_slice(&inner.data[start..end]);
                self.cursor += n as u64;
                self.shared.read_cursor.store(self.cursor, Relaxed);
                self.shared.stalled.store(false, Relaxed);
                return Ok(n);
            }

            // EOF: download finished and cursor is at or past end
            if inner.finished && self.cursor >= buf_end {
                return Ok(0);
            }

            // Partial download finished but cursor is before base_offset.
            // This happens when the stream completed after a Range restart
            // (buffer covers [base..EOF]) and a seek goes before base.
            // Clear finished to allow restart.
            if inner.finished && self.cursor < buf_start {
                crate::vprintln!(
                    "[BUFFER] Reopen: cursor={} < base={} (partial EOF). Requesting restart.",
                    self.cursor,
                    buf_start
                );
                inner.finished = false;
            }

            // Data not available - determine action:
            //   cursor < buf_start  -> data was discarded, need Range restart
            //   cursor > buf_end + SEEK_LOOKAHEAD -> too far ahead, Range restart
            //   cursor <= buf_end + SEEK_LOOKAHEAD -> wait for download to catch up
            if !inner.finished {
                let needs_restart =
                    self.cursor < buf_start || self.cursor > buf_end + SEEK_LOOKAHEAD;

                if needs_restart {
                    let is_forward = self.cursor > buf_end;
                    let restart_pos = self.cursor;

                    // Fires on every read-miss -> reveals the Range-restart storm cadence.
                    crate::vprintln3!(
                        "[BUFFER] read-miss {} | cursor={} base={} buf_end={} data={}KB target={:?} new={}",
                        if is_forward { "forward" } else { "backward" },
                        self.cursor,
                        buf_start,
                        buf_end,
                        inner.data.len() / 1024,
                        inner.restart_target,
                        inner.restart_target != Some(restart_pos),
                    );

                    if inner.restart_target != Some(restart_pos) {
                        crate::vprintln!(
                            "[BUFFER] Restart: {} | cursor={} base={} buf_end={} gap={}KB",
                            if is_forward { "forward" } else { "backward" },
                            self.cursor,
                            buf_start,
                            buf_end,
                            if self.cursor > buf_end {
                                (self.cursor - buf_end) / 1024
                            } else {
                                (buf_start - self.cursor) / 1024
                            }
                        );
                        inner.restart_target = Some(restart_pos);
                        drop(inner);
                        self.shared.async_notify.notify_one();
                        // This reader is about to wait on a refetch from a cold offset, which
                        // is what the boosted rate exists for. Asking here rather than at the
                        // seek keeps it to the restarts that actually lack bytes.
                        // Reads run on threads with no runtime, and GOVERNOR's init spawns a
                        // task: `main.rs` forcing it at startup is what keeps this from being
                        // the first touch. Tests reaching here need their own runtime.
                        crate::state::GOVERNOR
                            .buffer_progress()
                            .request_seek_boost();
                        inner = self.shared.inner.lock().unwrap_or_else(|e| e.into_inner());
                    }
                }
            }

            // Wait for data or state change (5s timeout to avoid deadlock)
            self.shared.stalled.store(true, Relaxed);
            let cursor_at_park = self.cursor;
            let (guard, wait_res) = self
                .shared
                .cvar
                .wait_timeout(inner, Duration::from_secs(5))
                .unwrap_or_else(|e| e.into_inner());
            inner = guard;
            if wait_res.timed_out() {
                // Waited the full timeout with no progress: genuine starvation.
                starved_cycles += 1;
                crate::vprintln3!(
                    "[BUFFER] STARVED: 5s timeout at cursor={cursor_at_park} | base={} buf_end={} finished={} target={:?}",
                    inner.base_offset,
                    inner.base_offset + inner.data.len() as u64,
                    inner.finished,
                    inner.restart_target,
                );
                if starved_cycles >= STALL_CYCLES {
                    // Only this reader is told. Setting `error` or `cancelled` would outlive the
                    // stall: three paths in `device.rs` respawn a decoder on this same buffer,
                    // two of them without consulting `is_reusable()`, so a shared flag would turn
                    // an ordinary device switch into a dead track. Leaving `Inner` untouched lets
                    // a later respawn read on normally if the writer was alive after all.
                    crate::verr!(
                        "[BUFFER] Starved {}s at cursor={cursor_at_park}, giving up on this read",
                        starved_cycles * 5
                    );
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "no data from the download for 30s",
                    ));
                }
            }
        }
    }
}

impl Seek for RamBuffer {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let total_len = {
            let inner = self.shared.inner.lock().unwrap_or_else(|e| e.into_inner());
            inner.total_len
        };

        let new_pos = match pos {
            SeekFrom::Start(offset) => offset as i64,
            SeekFrom::End(offset) => total_len as i64 + offset,
            SeekFrom::Current(offset) => self.cursor as i64 + offset,
        };

        if new_pos < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek to negative position",
            ));
        }

        let new_pos = new_pos as u64;
        if new_pos > total_len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("seek beyond total length: {} > {}", new_pos, total_len),
            ));
        }

        let old_cursor = self.cursor;
        self.cursor = new_pos;
        self.shared.read_cursor.store(new_pos, Relaxed);
        {
            let inner = self.shared.inner.lock().unwrap_or_else(|e| e.into_inner());
            crate::vprintln3!(
                "[BUFFER] Seek: cursor {old_cursor} -> {new_pos} | base={} buf_end={} total={}",
                inner.base_offset,
                inner.base_offset + inner.data.len() as u64,
                total_len
            );
        }
        Ok(new_pos)
    }
}

impl MediaSource for RamBuffer {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        // None skips symphonia's `probe_trailing` (gated on `byte_len().is_some()`), whose
        // EOF-then-0 seeks would discard the streaming pre-buffer and trigger wasted Range
        // restarts. FLAC duration comes from STREAMINFO and seek(End) uses `total_len`.
        None
    }
}

// --- Writer API ---

impl RamBufferWriter {
    /// Write decrypted data to the buffer. Returns true if data was accepted,
    /// false if discarded due to a pending restart.
    pub fn write_counted(&self, data: &[u8]) -> bool {
        let mut inner = self.shared.inner.lock().unwrap_or_else(|e| e.into_inner());
        // Discard writes if a restart is pending (stale data from old range)
        if inner.restart_target.is_some() {
            return false;
        }
        inner.data.extend_from_slice(data);
        let abs_written = inner.base_offset + inner.data.len() as u64;
        self.shared.written.store(abs_written, Relaxed);
        self.shared.cvar.notify_all();
        self.shared.async_notify.notify_one();
        true
    }

    /// Park the completed ciphertext staging file for the cache writer. Called
    /// once, at EOF - never on the per-chunk path.
    pub fn set_ciphertext(&self, staged: tempfile::NamedTempFile, len: u64) {
        let mut inner = self.shared.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.ciphertext = Some((staged, len));
    }

    pub fn finish(&self) {
        let mut inner = self.shared.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.finished = true;
        self.shared.cvar.notify_all();
        self.shared.async_notify.notify_one();
    }

    pub fn finish_with_error(&self, msg: String) {
        let mut inner = self.shared.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.error = Some(msg);
        inner.finished = true;
        self.shared.cvar.notify_all();
        self.shared.async_notify.notify_one();
    }

    pub fn is_cancelled(&self) -> bool {
        self.shared.cancelled_atomic.load(Relaxed)
    }

    /// Take the pending restart target (if any). Returns the absolute byte offset
    /// where the download should resume.
    pub fn take_restart_target(&self) -> Option<u64> {
        let mut inner = self.shared.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.restart_target.take()
    }

    /// Reset the buffer for a new Range request starting at `new_offset`.
    pub fn reset_for_range(&self, new_offset: u64) {
        let mut inner = self.shared.inner.lock().unwrap_or_else(|e| e.into_inner());
        crate::vprintln3!(
            "[BUFFER] reset_for_range: base {} -> {} | discarding {}KB",
            inner.base_offset,
            new_offset,
            inner.data.len() / 1024,
        );
        inner.data.clear();
        inner.base_offset = new_offset;
        inner.finished = false;
        inner.error = None;
        self.shared.written.store(new_offset, Relaxed);
        self.shared.cvar.notify_all();
    }

    /// Wait for a restart request or cancellation (async).
    pub async fn wait_for_restart_or_cancel(&self) {
        loop {
            {
                let inner = self.shared.inner.lock().unwrap_or_else(|e| e.into_inner());
                if inner.restart_target.is_some() || inner.cancelled {
                    return;
                }
            }
            self.shared.async_notify.notified().await;
        }
    }

    /// Check if a restart is pending without consuming it.
    pub fn has_restart_pending(&self) -> bool {
        let inner = self.shared.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.restart_target.is_some()
    }
}

impl Drop for RamBufferWriter {
    fn drop(&mut self) {
        // If the download ended without finishing (cancelled mid-stream, an error
        // path that returned, or a panic), retire the buffer; a reader blocked at
        // the frontier bails instead of hanging. No-op once finished.
        let mut inner = self.shared.inner.lock().unwrap_or_else(|e| e.into_inner());
        if !inner.finished {
            inner.cancelled = true;
            self.shared.cancelled_atomic.store(true, Relaxed);
            self.shared.cvar.notify_all();
            self.shared.async_notify.notify_one();
        }
    }
}

#[cfg(test)]
#[path = "../../tests/unit/player/buffer.rs"]
mod tests;
