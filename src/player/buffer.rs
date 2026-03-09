use std::io::{self, Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use symphonia::core::io::MediaSource;

const INITIAL_BUFFER_CAP: usize = 2 * 1024 * 1024; // 2 MB

/// When the read cursor is slightly ahead of buf_end, wait for the sequential
/// download to catch up instead of triggering a new Range restart.
/// At governed ~443 KB/s, 32 KB arrives in ~72 ms; a Range restart costs
/// ~30 ms TTFB, so keep this small to avoid waiting longer than restarting.
const SEEK_LOOKAHEAD: u64 = 32 * 1024; // 32 KB

struct Inner {
    data: Vec<u8>,
    base_offset: u64, // file offset where data[0] starts
    total_len: u64,   // total file size
    finished: bool,
    cancelled: bool,
    error: Option<String>,
    restart_target: Option<u64>, // signal download thread to restart
}

/// Shared state between readers, writers, and the async download loop.
struct SharedState {
    inner: Mutex<Inner>,
    cvar: Condvar,
    /// Bytes written (absolute file offset = base_offset + data.len()).
    written: AtomicU64,
    base_offset_atomic: AtomicU64,
    finished_atomic: AtomicBool,
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
}

/// Write-side handle for the async download task.
pub struct RamBufferWriter {
    shared: Arc<SharedState>,
}

impl RamBuffer {
    /// Create a new empty buffer for streaming.
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
            }),
            cvar: Condvar::new(),
            written: AtomicU64::new(0),
            base_offset_atomic: AtomicU64::new(0),
            finished_atomic: AtomicBool::new(false),
            cancelled_atomic: AtomicBool::new(false),
            read_cursor: AtomicU64::new(0),
            async_notify: tokio::sync::Notify::new(),
            stalled: AtomicBool::new(false),
        });

        let buffer = RamBuffer {
            shared: shared.clone(),
            cursor: 0,
        };
        let writer = RamBufferWriter { shared };
        (buffer, writer)
    }

    /// Create a buffer with all data already present (cache hit or preload).
    pub fn from_complete(data: Vec<u8>) -> Self {
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
            }),
            cvar: Condvar::new(),
            written: AtomicU64::new(total_len),
            base_offset_atomic: AtomicU64::new(0),
            finished_atomic: AtomicBool::new(true),
            cancelled_atomic: AtomicBool::new(false),
            read_cursor: AtomicU64::new(0),
            async_notify: tokio::sync::Notify::new(),
            stalled: AtomicBool::new(false),
        });

        RamBuffer { shared, cursor: 0 }
    }

    /// Cancel the streaming download.
    pub fn cancel(&self) {
        let mut inner = self.shared.inner.lock().expect("RamBuffer lock poisoned");
        inner.cancelled = true;
        self.shared.cancelled_atomic.store(true, Relaxed);
        self.shared.cvar.notify_all();
        self.shared.async_notify.notify_one();
    }

    /// Returns true if the entire file has been downloaded without error.
    pub fn is_complete(&self) -> bool {
        let inner = self.shared.inner.lock().expect("RamBuffer lock poisoned");
        inner.finished && inner.error.is_none() && inner.base_offset == 0
    }

    /// Total file size.
    pub fn total_len(&self) -> u64 {
        let inner = self.shared.inner.lock().expect("RamBuffer lock poisoned");
        inner.total_len
    }

    /// Take ownership of the complete data buffer (only if fully downloaded from offset 0).
    /// Returns None if incomplete or base_offset != 0.
    pub fn take_data(&self) -> Option<Vec<u8>> {
        let mut inner = self.shared.inner.lock().expect("RamBuffer lock poisoned");
        if inner.finished
            && inner.error.is_none()
            && !inner.data.is_empty()
            && inner.base_offset == 0
        {
            Some(std::mem::take(&mut inner.data))
        } else {
            None
        }
    }

    /// Current amount of data written (absolute byte offset).
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
        let mut inner = self.shared.inner.lock().expect("RamBuffer lock poisoned");

        loop {
            if inner.cancelled {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "streaming cancelled",
                ));
            }

            if let Some(ref err) = inner.error {
                return Err(io::Error::other(err.clone()));
            }

            let buf_start = inner.base_offset;
            let buf_end = inner.base_offset + inner.data.len() as u64;

            // Data available in buffer
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
                self.shared.finished_atomic.store(false, Relaxed);
            }

            // Data not available — determine action:
            //   cursor < buf_start  → data was discarded, need Range restart
            //   cursor > buf_end + SEEK_LOOKAHEAD → too far ahead, Range restart
            //   cursor <= buf_end + SEEK_LOOKAHEAD → wait for download to catch up
            if !inner.finished {
                let needs_restart =
                    self.cursor < buf_start || self.cursor > buf_end + SEEK_LOOKAHEAD;

                if needs_restart {
                    let is_forward = self.cursor > buf_end;
                    let restart_pos = self.cursor;

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
                        inner = self.shared.inner.lock().expect("RamBuffer lock poisoned");
                    }
                }
            }

            // Wait for data or state change (5s timeout to avoid deadlock)
            self.shared.stalled.store(true, Relaxed);
            let (guard, _) = self
                .shared
                .cvar
                .wait_timeout(inner, Duration::from_secs(5))
                .expect("RamBuffer condvar lock poisoned");
            inner = guard;
        }
    }
}

impl Seek for RamBuffer {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let total_len = {
            let inner = self.shared.inner.lock().expect("RamBuffer lock poisoned");
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

        self.cursor = new_pos;
        self.shared.read_cursor.store(new_pos, Relaxed);
        Ok(new_pos)
    }
}

impl MediaSource for RamBuffer {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        let inner = self.shared.inner.lock().expect("RamBuffer lock poisoned");
        Some(inner.total_len)
    }
}

// --- Writer API ---

impl RamBufferWriter {
    /// Write decrypted data to the buffer. Returns true if data was accepted,
    /// false if discarded due to a pending restart.
    pub fn write_counted(&self, data: &[u8]) -> bool {
        let mut inner = self.shared.inner.lock().expect("RamBuffer lock poisoned");
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

    /// Mark the download as finished (success).
    pub fn finish(&self) {
        let mut inner = self.shared.inner.lock().expect("RamBuffer lock poisoned");
        inner.finished = true;
        self.shared.finished_atomic.store(true, Relaxed);
        self.shared.cvar.notify_all();
        self.shared.async_notify.notify_one();
    }

    /// Mark the download as finished with an error.
    pub fn finish_with_error(&self, msg: String) {
        let mut inner = self.shared.inner.lock().expect("RamBuffer lock poisoned");
        inner.error = Some(msg);
        inner.finished = true;
        self.shared.finished_atomic.store(true, Relaxed);
        self.shared.cvar.notify_all();
        self.shared.async_notify.notify_one();
    }

    /// Check if the reader has cancelled the download.
    pub fn is_cancelled(&self) -> bool {
        self.shared.cancelled_atomic.load(Relaxed)
    }

    /// Take the pending restart target (if any). Returns the absolute byte offset
    /// where the download should resume.
    pub fn take_restart_target(&self) -> Option<u64> {
        let mut inner = self.shared.inner.lock().expect("RamBuffer lock poisoned");
        inner.restart_target.take()
    }

    /// Reset the buffer for a new Range request starting at `new_offset`.
    pub fn reset_for_range(&self, new_offset: u64) {
        let mut inner = self.shared.inner.lock().expect("RamBuffer lock poisoned");
        inner.data.clear();
        inner.base_offset = new_offset;
        inner.finished = false;
        inner.error = None;
        self.shared.base_offset_atomic.store(new_offset, Relaxed);
        self.shared.written.store(new_offset, Relaxed);
        self.shared.finished_atomic.store(false, Relaxed);
        self.shared.cvar.notify_all();
    }

    /// Wait for a restart request or cancellation (async).
    pub async fn wait_for_restart_or_cancel(&self) {
        loop {
            {
                let inner = self.shared.inner.lock().expect("RamBuffer lock poisoned");
                if inner.restart_target.is_some() || inner.cancelled {
                    return;
                }
            }
            self.shared.async_notify.notified().await;
        }
    }

    /// Check if a restart is pending without consuming it.
    pub fn has_restart_pending(&self) -> bool {
        let inner = self.shared.inner.lock().expect("RamBuffer lock poisoned");
        inner.restart_target.is_some()
    }
}
