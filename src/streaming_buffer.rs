use crate::bandwidth::BufferProgress;
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, Condvar, Mutex};

const BUFFER_AHEAD_LIMIT: u64 = 2 * 1024 * 1024; // 2 MB
/// Adaptive restart threshold: 3 seconds of audio, clamped to [256 KB, 1 MB].
/// Falls back to the fixed 1 MB when bitrate is unknown.
fn restart_threshold(bitrate_bps: u64) -> u64 {
    const FLOOR: u64 = 256 * 1024;
    const CEIL: u64 = 1024 * 1024;
    const DURATION_SECS: u64 = 3;

    if bitrate_bps == 0 {
        return CEIL; // conservative fallback
    }
    bitrate_bps.saturating_mul(DURATION_SECS).clamp(FLOOR, CEIL)
}

/// Early restart: force a Range restart for any forward gap above this floor.
/// A restart with boost (~280ms) is almost always faster than waiting for
/// linear download at 5× bitrate to fill gaps of this size.
const EARLY_RESTART_FLOOR: u64 = 256 * 1024;

struct StaleRange {
    base_offset: u64,
    data: Vec<u8>,
}

struct Inner {
    data: Vec<u8>,
    total_len: u64,
    base_offset: u64,
    finished: bool,
    cancelled: bool,
    error: Option<String>,
    read_pos: u64,
    restart_target: Option<u64>,
    /// Saved buffer from before the last Range restart.
    /// Enables instant backward seeks into recently played data.
    stale: Option<StaleRange>,
    /// When true, the download loop skips padding (stale cache continuation).
    restart_skip_padding: bool,
}

pub struct StreamingBuffer {
    inner: Arc<(Mutex<Inner>, Condvar)>,
    writer_notify: Arc<tokio::sync::Notify>,
    read_pos: u64,
    buffer_progress: Option<Arc<BufferProgress>>,
}

pub struct StreamingBufferWriter {
    inner: Arc<(Mutex<Inner>, Condvar)>,
    writer_notify: Arc<tokio::sync::Notify>,
    buffer_progress: Option<Arc<BufferProgress>>,
}

impl StreamingBuffer {
    pub fn new(
        total_len: u64,
        buffer_progress: Option<Arc<BufferProgress>>,
    ) -> (Self, StreamingBufferWriter) {
        if let Some(ref bp) = buffer_progress {
            bp.total_len.store(total_len, Relaxed);
        }

        let inner = Arc::new((
            Mutex::new(Inner {
                data: Vec::with_capacity(total_len as usize),
                total_len,
                base_offset: 0,
                finished: false,
                cancelled: false,
                error: None,
                read_pos: 0,
                restart_target: None,
                stale: None,
                restart_skip_padding: false,
            }),
            Condvar::new(),
        ));
        let writer_notify = Arc::new(tokio::sync::Notify::new());

        let buffer = StreamingBuffer {
            inner: inner.clone(),
            writer_notify: writer_notify.clone(),
            read_pos: 0,
            buffer_progress: buffer_progress.clone(),
        };

        let writer = StreamingBufferWriter {
            inner,
            writer_notify,
            buffer_progress,
        };

        (buffer, writer)
    }

    pub fn cancel(&self) {
        let (lock, cvar) = &*self.inner;
        let mut inner = lock.lock().unwrap();
        inner.cancelled = true;
        cvar.notify_all();
        self.writer_notify.notify_one();
    }

    pub fn is_complete(&self) -> bool {
        let (lock, _) = &*self.inner;
        let inner = lock.lock().unwrap();
        inner.finished && inner.error.is_none()
    }

    pub fn total_len(&self) -> u64 {
        let (lock, _) = &*self.inner;
        let inner = lock.lock().unwrap();
        inner.total_len
    }

    pub fn take_data(&self) -> Option<Vec<u8>> {
        let (lock, _) = &*self.inner;
        let mut inner = lock.lock().unwrap();
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

    pub fn new_reader(&self) -> Self {
        StreamingBuffer {
            inner: self.inner.clone(),
            writer_notify: self.writer_notify.clone(),
            read_pos: 0,
            buffer_progress: self.buffer_progress.clone(),
        }
    }
}

impl Read for StreamingBuffer {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let (lock, cvar) = &*self.inner;
        let mut inner = lock.lock().unwrap();

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
            if self.read_pos >= buf_start && self.read_pos < buf_end {
                let start = (self.read_pos - inner.base_offset) as usize;
                let end = std::cmp::min(start + buf.len(), inner.data.len());
                let n = end - start;
                buf[..n].copy_from_slice(&inner.data[start..end]);
                self.read_pos += n as u64;
                if self.read_pos > inner.read_pos {
                    inner.read_pos = self.read_pos;
                    if let Some(ref bp) = self.buffer_progress {
                        bp.read_pos.store(self.read_pos, Relaxed);
                    }
                }
                drop(inner);
                self.writer_notify.notify_one();
                return Ok(n);
            }

            if inner.finished {
                return Ok(0);
            }

            inner = cvar.wait(inner).unwrap();
        }
    }
}

impl Seek for StreamingBuffer {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let total_len = {
            let (lock, _) = &*self.inner;
            lock.lock().unwrap().total_len
        };

        let new_pos = match pos {
            SeekFrom::Start(offset) => offset as i64,
            SeekFrom::End(offset) => total_len as i64 + offset,
            SeekFrom::Current(offset) => self.read_pos as i64 + offset,
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

        self.read_pos = new_pos;

        {
            let (lock, _) = &*self.inner;
            let mut inner = lock.lock().unwrap();

            // Update inner read_pos so backpressure unblocks when seeking forward.
            if new_pos > inner.read_pos {
                inner.read_pos = new_pos;
            }

            let buf_end = inner.base_offset + inner.data.len() as u64;

            // Decide whether to trigger a Range restart
            if !inner.finished && !inner.cancelled {
                let bitrate = self
                    .buffer_progress
                    .as_ref()
                    .map(|bp| bp.bitrate_bps.load(Relaxed))
                    .unwrap_or(0);
                let threshold = restart_threshold(bitrate);
                let gap = new_pos.saturating_sub(buf_end);
                let would_restart =
                    new_pos < inner.base_offset || gap >= threshold || gap >= EARLY_RESTART_FLOOR;

                let mut restored = false;

                // Check stale cache before issuing a network restart
                if would_restart && let Some(ref stale) = inner.stale {
                    let stale_end = stale.base_offset + stale.data.len() as u64;
                    if new_pos >= stale.base_offset && new_pos < stale_end {
                        // Swap current ↔ stale — instant restore, no network
                        let old_stale = inner.stale.take().unwrap();
                        if !inner.data.is_empty() {
                            inner.stale = Some(StaleRange {
                                base_offset: inner.base_offset,
                                data: std::mem::take(&mut inner.data),
                            });
                        }
                        inner.base_offset = old_stale.base_offset;
                        inner.data = old_stale.data;
                        let continue_from = inner.base_offset + inner.data.len() as u64;
                        inner.restart_target = Some(continue_from);
                        inner.restart_skip_padding = true;
                        inner.finished = false;
                        eprintln!(
                            "[SEEK]   Cache hit: pos={} restored=[{}..{}]",
                            new_pos, inner.base_offset, continue_from
                        );
                        if let Some(ref bp) = self.buffer_progress {
                            bp.written.store(continue_from, Relaxed);
                        }
                        restored = true;
                    }
                }

                if would_restart && !restored {
                    eprintln!(
                        "[SEEK]   Range restart: pos={} available=[{}..{}] gap={}KB",
                        new_pos,
                        inner.base_offset,
                        buf_end,
                        if new_pos >= buf_end {
                            (new_pos - buf_end) / 1024
                        } else {
                            (inner.base_offset - new_pos) / 1024
                        }
                    );
                    // Save current data as stale before clearing
                    if !inner.data.is_empty() {
                        inner.stale = Some(StaleRange {
                            base_offset: inner.base_offset,
                            data: std::mem::take(&mut inner.data),
                        });
                    }
                    inner.base_offset = new_pos;
                    inner.restart_target = Some(new_pos);
                    inner.restart_skip_padding = false;
                    if let Some(ref bp) = self.buffer_progress {
                        bp.written.store(new_pos, Relaxed);
                        bp.request_seek_boost();
                    }
                }

                if !would_restart && new_pos >= buf_end && !inner.finished {
                    eprintln!(
                        "[SEEK]   Forward within threshold: pos={} buf_end={} gap={}KB (threshold={}KB)",
                        new_pos,
                        buf_end,
                        (new_pos - buf_end) / 1024,
                        threshold / 1024
                    );
                }
            }
        }

        if let Some(ref bp) = self.buffer_progress {
            bp.read_pos.store(new_pos, Relaxed);
        }
        // Wake writer in case it was blocked by backpressure or needs to handle restart
        self.writer_notify.notify_one();

        Ok(new_pos)
    }
}

impl StreamingBufferWriter {
    pub fn write(&self, data: &[u8]) {
        let (lock, cvar) = &*self.inner;
        let mut inner = lock.lock().unwrap();
        if inner.restart_target.is_some() {
            return; // Restart pending — discard stale writes
        }
        inner.data.extend_from_slice(data);
        if let Some(ref bp) = self.buffer_progress {
            bp.written
                .store(inner.base_offset + inner.data.len() as u64, Relaxed);
        }
        cvar.notify_all();
    }

    pub fn finish(&self) {
        let (lock, cvar) = &*self.inner;
        let mut inner = lock.lock().unwrap();
        inner.finished = true;
        cvar.notify_all();
    }

    pub fn finish_with_error(&self, msg: String) {
        let (lock, cvar) = &*self.inner;
        let mut inner = lock.lock().unwrap();
        inner.error = Some(msg);
        inner.finished = true;
        cvar.notify_all();
    }

    pub fn is_cancelled(&self) -> bool {
        let (lock, _) = &*self.inner;
        let inner = lock.lock().unwrap();
        inner.cancelled
    }

    pub fn has_restart_pending(&self) -> bool {
        let (lock, _) = &*self.inner;
        let inner = lock.lock().unwrap();
        inner.restart_target.is_some()
    }

    pub fn take_restart_target(&self) -> Option<(u64, bool)> {
        let (lock, _) = &*self.inner;
        let mut inner = lock.lock().unwrap();
        inner.restart_target.take().map(|t| {
            let skip = inner.restart_skip_padding;
            inner.restart_skip_padding = false;
            (t, skip)
        })
    }

    pub fn reset_for_range(&self, new_offset: u64) {
        let (lock, cvar) = &*self.inner;
        let mut inner = lock.lock().unwrap();
        let current_end = inner.base_offset + inner.data.len() as u64;
        if new_offset != current_end {
            // Different range — clear and restart
            inner.data.clear();
            inner.base_offset = new_offset;
        }
        // else: continuation from stale restore — keep existing data
        inner.finished = false;
        inner.error = None;
        if let Some(ref bp) = self.buffer_progress {
            bp.written
                .store(inner.base_offset + inner.data.len() as u64, Relaxed);
        }
        cvar.notify_all();
    }

    pub fn bitrate_bps(&self) -> u64 {
        self.buffer_progress
            .as_ref()
            .map(|bp| bp.bitrate_bps.load(Relaxed))
            .unwrap_or(0)
    }

    pub async fn wait_if_buffer_full(&self) {
        loop {
            {
                let (lock, _) = &*self.inner;
                let inner = lock.lock().unwrap();
                if inner.restart_target.is_some() || inner.cancelled {
                    return;
                }
                let absolute_written = inner.base_offset + inner.data.len() as u64;
                let ahead = absolute_written.saturating_sub(inner.read_pos);
                if ahead < BUFFER_AHEAD_LIMIT {
                    return;
                }
            }
            self.writer_notify.notified().await;
        }
    }

    pub async fn wait_for_restart_or_cancel(&self) {
        loop {
            {
                let (lock, _) = &*self.inner;
                let inner = lock.lock().unwrap();
                if inner.restart_target.is_some() || inner.cancelled {
                    return;
                }
            }
            self.writer_notify.notified().await;
        }
    }
}
