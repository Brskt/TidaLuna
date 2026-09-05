use std::io::{self, Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use symphonia::core::io::MediaSource;
use tokio_util::sync::CancellationToken;

const INITIAL_BUFFER_CAP: usize = 2 * 1024 * 1024; // 2 MB

/// When the read cursor is slightly ahead of buf_end, wait for the sequential
/// download to catch up instead of triggering a new Range restart. At the
/// governed download rate this lookahead arrives in well under a restart's
/// TTFB; keep it small enough that waiting never loses to restarting.
const SEEK_LOOKAHEAD: u64 = 32 * 1024; // 32 KB

/// Why a download gave up. Kept alongside the message because the two answers the
/// reader owes are opposite: a dead connection raises the no-connection banner and
/// holds the queue where it is, an unusable source reports a media error and lets the
/// queue advance. Reported as one kind, they were indistinguishable, and every source
/// failure borrowed the network's answer.
#[derive(Clone, Copy)]
pub enum DownloadFailure {
    /// The connection is gone: a send that never left, or a reconnect budget spent.
    Network,
    /// The connection worked and its answer is unusable: a rejected status, a key that
    /// does not decrypt. The same request would fail the same way.
    Source,
}

/// Why a read stopped because someone asked. Carried INSIDE the error rather than encoded as its
/// kind, because `ErrorKind` is shared with std and symphonia, which mints `Other` for two
/// meanings of its own: a stop deduced from a kind is a guess, read off the payload it is a fact.
///
/// The kind stays `Other` all the same: `Interrupted` is the one kind symphonia's
/// `read_buf_exact` retries without limit, and both stops here are latched, so every retry
/// returns at once and spins the decode thread at full CPU.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReadStop {
    /// The whole buffer was cancelled: no reader of it is served again.
    StreamCancelled,
    /// This reader alone was retired (a stale exclusive stream after a mode switch) while
    /// the buffer goes on serving whoever else holds it.
    ReaderRetired,
}

impl ReadStop {
    /// Reads the stop back off an error a decoder caught, and owns that discrimination: no
    /// consumer has to re-derive it from a kind.
    pub fn from_io(err: &io::Error) -> Option<Self> {
        err.get_ref()?.downcast_ref::<Self>().copied()
    }
}

impl std::fmt::Display for ReadStop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::StreamCancelled => "streaming cancelled",
            Self::ReaderRetired => "reader retired",
        })
    }
}

impl std::error::Error for ReadStop {}

/// Who a download belongs to, which settles the bandwidth class its bytes are charged to
/// and where a reconnect looks for a credential.
///
/// A property of the BUFFER, not of the task filling it: a buffer staged ahead of the listener
/// becomes the buffer of the track being listened to, still filling, and the answer changes with
/// it. Held by value in the task it was captured from, it could not.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DownloadOwner {
    /// The track being listened to. It owns `CURRENT_TRACK`; a same-track re-assert can
    /// re-sign this task's url underneath it, and every reconnect has to re-read it.
    Playback,
    /// A track staged ahead of the listener. It owns no such slot, and re-staging goes
    /// through `cancel_preload`: a fresh url arrives with a fresh task.
    Preload,
}

impl DownloadOwner {
    fn as_bits(self) -> u8 {
        match self {
            Self::Playback => 0,
            Self::Preload => 1,
        }
    }

    fn from_bits(bits: u8) -> Self {
        match bits {
            0 => Self::Playback,
            _ => Self::Preload,
        }
    }
}

/// The download filling a streaming buffer: who owns it, and the one door that stops it.
///
/// Both live here rather than in a slot beside the buffer, because the buffer is what gets
/// handed over: an external slot has to be kept in step with every handover, and the two that
/// existed were not, adoption dropping the token and leaving a download nothing could stop.
struct DownloadHandle {
    owner: AtomicU8,
    cancel: CancellationToken,
}

impl DownloadFailure {
    /// The two kinds a reader can tell apart. `Other` stays reserved for the deliberate
    /// stops above, which announce nothing.
    fn kind(self) -> io::ErrorKind {
        match self {
            Self::Network => io::ErrorKind::ConnectionAborted,
            Self::Source => io::ErrorKind::InvalidData,
        }
    }
}

impl std::fmt::Display for DownloadFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Network => "connection gone",
            Self::Source => "source unusable",
        })
    }
}

/// Where a wait for the first `target` bytes stands, as one answer.
///
/// A waiter needs two facts a byte count cannot carry, and needs them as one: whether the head
/// is there, and if not whether the download can still deliver it. Asked separately they can
/// disagree, a writer that lands its last chunk and then ends changing both between the reads.
#[derive(Clone, Copy)]
pub enum HeadStatus {
    /// `target` bytes are present from offset zero: the head can be probed.
    Landed,
    /// Short of `target`, and the download that fills this buffer is still running.
    Filling,
    /// Short of `target`, and no further byte will ever arrive. Carries the failure where
    /// there was one; a clean end under the announced length, and a writer that went away,
    /// both report none.
    Ended(Option<DownloadFailure>),
}

struct Inner {
    data: Vec<u8>,
    base_offset: u64,
    total_len: u64,
    finished: bool,
    cancelled: bool,
    error: Option<(DownloadFailure, String)>,
    restart_target: Option<u64>,
    /// Ciphertext staging file for the disk cache, with its byte count. The download
    /// loop owns it while streaming and parks it here only at EOF: no per-chunk
    /// write ever takes this lock.
    ciphertext: Option<(tempfile::NamedTempFile, u64)>,
}

impl Inner {
    /// The whole announced file, from offset zero, with no failure.
    ///
    /// Written once and shared by the three consumers below, because `finished` alone does not
    /// answer this: `finish()` says the STREAM ended, not that the file arrived, and an HTTP/2
    /// `RST_STREAM` with `NO_ERROR` mid-body or a reconnect answered `416` both end a short
    /// transfer with no error set. Kept whole through that, a truncated track is indexed in the
    /// disk cache as complete and served as valid. `>=` rather than `==`, a body longer than
    /// announced being a different complaint that must never make a complete file look partial.
    fn is_whole(&self) -> bool {
        self.finished
            && self.error.is_none()
            && self.base_offset == 0
            && self.data.len() as u64 >= self.total_len
    }
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
    /// The download filling this buffer. A buffer that arrived complete carries one that
    /// is already cancelled rather than none at all: every accessor stays total, and a
    /// download whose handle went missing cannot be mistaken for one that needs no
    /// stopping, which is the shape the bug being fixed here had.
    download: DownloadHandle,
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
    // Per-reader stop: `read` fails for this reader alone, without touching the shared
    // `cancelled` (which retires every reader). Drops a stale exclusive reader on
    // a mode switch, stopping it from fighting the new shared reader.
    reader_cancel: Option<Arc<AtomicBool>>,
}

/// Write-side handle for the async download task.
pub struct RamBufferWriter {
    shared: Arc<SharedState>,
}

impl RamBuffer {
    /// `owner` is who the download starts out belonging to; a staged track can be adopted
    /// by playback later through [`RamBuffer::adopt_as_playback`]. `cancel` is supplied by
    /// the caller rather than minted here, because the caller already holds a slot that
    /// has to stop this same download: one token, however it is reached.
    pub fn new(
        total_len: u64,
        owner: DownloadOwner,
        cancel: CancellationToken,
    ) -> (Self, RamBufferWriter) {
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
            download: DownloadHandle {
                owner: AtomicU8::new(owner.as_bits()),
                cancel,
            },
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
            // Complete on arrival: cancelled from the start, because there is no task to
            // stop and nothing further will be written.
            download: DownloadHandle {
                owner: AtomicU8::new(DownloadOwner::Playback.as_bits()),
                cancel: {
                    let token = CancellationToken::new();
                    token.cancel();
                    token
                },
            },
        });

        RamBuffer {
            shared,
            cursor: 0,
            reader_cancel: None,
        }
    }

    /// A streaming pair for tests about read, write and restart behaviour, where the
    /// download's owner is not what is under test. Anything asserting on adoption builds
    /// its pair through [`RamBuffer::new`] and says which owner it starts from.
    #[cfg(test)]
    pub(crate) fn new_for_test(total_len: u64) -> (Self, RamBufferWriter) {
        Self::new(total_len, DownloadOwner::Playback, CancellationToken::new())
    }

    /// Attach a per-reader stop signal: when set, this reader's `read` fails, leaving
    /// other readers untouched. This is what lets a buffer outlive the decoder that was
    /// reading it, for the next decoder to pick up.
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
        drop(inner);
        // One door, not two. The flag alone is only observed between awaits, and the
        // ciphertext flush is a blocking write of up to a megabyte that nothing races;
        // the token drops the whole stream where it stands.
        self.cancel_download();
    }

    /// Whether this buffer was cancelled, asked from the reading side. The decode thread needs
    /// it where symphonia throws away the read error that would have named the stop: its probe
    /// scans with `while let Ok(byte) = mss.read_byte()` and reports a missing format reader
    /// instead of the failure it swallowed. Safe to ask after a read came back empty, that read
    /// having taken the lock `cancel` holds while setting this.
    pub fn is_cancelled(&self) -> bool {
        self.shared.cancelled_atomic.load(Relaxed)
    }

    /// Stop the download filling this buffer, leaving what has already landed readable.
    /// A buffer that arrived complete is already stopped.
    pub fn cancel_download(&self) {
        self.shared.download.cancel.cancel();
    }

    /// Hand the download filling this buffer over to playback. Called where a staged
    /// track becomes the track being listened to, while its bytes are still arriving:
    /// from here its traffic is the listener's, and a reconnect re-reads the credential
    /// that `CURRENT_TRACK` now carries for it.
    pub fn adopt_as_playback(&self) {
        self.shared
            .download
            .owner
            .store(DownloadOwner::Playback.as_bits(), Relaxed);
    }

    /// True when both handles name the same download. Distinguishes a genuine track
    /// change from a rebuild that reinstalls a clone of the buffer already current.
    pub fn is_same_stream(&self, other: &RamBuffer) -> bool {
        Arc::ptr_eq(&self.shared, &other.shared)
    }

    /// Returns true if the entire file has been downloaded without error.
    pub fn is_complete(&self) -> bool {
        let inner = self.shared.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.is_whole()
    }

    /// True once the download feeding this buffer has ended in failure.
    ///
    /// `is_complete` cannot answer this, folding a dead download in with one still arriving:
    /// both are merely "not the whole file". A holder weighing whether a published attempt is
    /// still worth anything needs the narrower fact by itself. `head_status` will not serve
    /// either, answering `Landed` past its byte target whatever else is true.
    pub fn has_failed(&self) -> bool {
        let inner = self.shared.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.error.is_some()
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
        // read fails; a finished buffer that was cancelled reads no better than an empty
        // one.
        inner.is_whole() && !inner.cancelled && !inner.data.is_empty()
    }

    pub fn total_len(&self) -> u64 {
        let inner = self.shared.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.total_len
    }

    /// Take the ciphertext staging file the download parked at EOF. Gated on the same
    /// completeness as `is_complete()`: a partial, short or Range-restarted download never
    /// yields one; the caller cannot index as whole what arrived in part.
    pub fn take_ciphertext(&self) -> Option<(tempfile::NamedTempFile, u64)> {
        let mut inner = self.shared.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.is_whole() {
            inner.ciphertext.take()
        } else {
            None
        }
    }

    pub fn written(&self) -> u64 {
        self.shared.written.load(Relaxed)
    }

    /// Answer a head wait: are `target` bytes here, can they still come, or is it over?
    ///
    /// Both facts come out of ONE critical section, the way [`Read::read`] takes them, because
    /// read as two `Relaxed` atomics they race: a writer that appends its last chunk and then
    /// finishes, between the two loads, is seen as ended while short, and a head already in
    /// hand gets refused. Bytes are decided before the end, the caller asking whether it can
    /// probe rather than whether the download is alive. A `base_offset` past zero refuses
    /// whatever `written` says: after a Range restart the head is no longer in `data`.
    pub fn head_status(&self, target: u64) -> HeadStatus {
        let inner = self.shared.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.base_offset == 0 && inner.data.len() as u64 >= target {
            return HeadStatus::Landed;
        }
        if inner.finished || inner.cancelled {
            return HeadStatus::Ended(inner.error.as_ref().map(|(cause, _)| *cause));
        }
        HeadStatus::Filling
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
        // side that knows anyone is HARMED: the writer goes quiet for reasons that are entirely
        // correct (parked after a partial EOF, or held back by the governor), and a clock on its
        // idleness cannot tell those from a dead server. Counted locally, a starved reader never
        // leaving this call. Six cycles, the floor being the download's own retry budget: eight
        // reconnects backing off 250ms x attempt sums to 9s before the writer reports it itself.
        const STALL_CYCLES: u32 = 6;
        let mut starved_cycles: u32 = 0;

        let mut inner = self.shared.inner.lock().unwrap_or_else(|e| e.into_inner());

        loop {
            // Neither stop below may report `Interrupted`: symphonia's `read_buf_exact`
            // retries that kind without limit, and both flags here are latched; every
            // retry returns at once and spins the decode thread at full CPU inside
            // `next_packet`. Whoever joins that thread (`cancel_crossfade` does, on the
            // sequential control thread) then never gets it back.
            if inner.cancelled {
                return Err(io::Error::other(ReadStop::StreamCancelled));
            }

            // Retired reader (e.g. a stale exclusive stream after a mode switch):
            // bail without disturbing the shared state the live reader depends on.
            if let Some(ref c) = self.reader_cancel
                && c.load(Relaxed)
            {
                return Err(io::Error::other(ReadStop::ReaderRetired));
            }

            // Three ways out reach a reader from here, and the decoder owes each a different
            // answer: the named `ReadStop` says nothing because someone asked for it,
            // `ConnectionAborted` raises the no-connection banner and holds the queue, and
            // `InvalidData` reports a media error and lets it advance. The download gives up
            // for six reasons and only two are the network: reported as one kind, an expired
            // url's 403 announced "no internet" over a healthy connection.
            if let Some((cause, ref err)) = inner.error {
                return Err(io::Error::new(cause.kind(), err.clone()));
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
                        // This reader is about to wait on a refetch from a cold offset, what
                        // the boosted rate exists for; asking here rather than at the seek
                        // keeps it to the restarts that actually lack bytes. Reads run on
                        // threads with no runtime and GOVERNOR's init spawns a task, so
                        // `main.rs` forces it at startup and tests need their own runtime.
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
                    // two of them without consulting `is_reusable()`; a shared flag would turn
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

    /// End the download on a failure. `cause` decides what the listener is told: it
    /// answers "is the connection gone", never "how bad does this look".
    pub fn finish_with_error(&self, cause: DownloadFailure, msg: String) {
        let mut inner = self.shared.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.error = Some((cause, msg));
        inner.finished = true;
        self.shared.cvar.notify_all();
        self.shared.async_notify.notify_one();
    }

    pub fn is_cancelled(&self) -> bool {
        self.shared.cancelled_atomic.load(Relaxed)
    }

    /// Who the bytes being written belong to right now. Read per chunk and per reconnect
    /// rather than captured at spawn: adoption changes the answer mid-download, and the
    /// two decisions that follow from it (which bucket pays, which url reconnects) have
    /// to change with it.
    pub fn owner(&self) -> DownloadOwner {
        DownloadOwner::from_bits(self.shared.download.owner.load(Relaxed))
    }

    /// The token stopping this download, for the task filling the buffer to be raced
    /// against it from the outside.
    pub fn cancel_token(&self) -> CancellationToken {
        self.shared.download.cancel.clone()
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
