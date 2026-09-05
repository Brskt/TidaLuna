use crate::audio::bandwidth::TrafficClass;
use crate::audio::decrypt::FlacDecryptor;
use crate::player::buffer::{DownloadFailure, DownloadOwner, HeadStatus, RamBufferWriter};
use crate::state::{GOVERNOR, HTTP_CLIENT_PRELOAD, PreloadedTrack, RetainedTrack, TrackInfo};
use futures_util::StreamExt;
use tokio_util::sync::CancellationToken;

const PRELOAD_MAX_BYTES: usize = 32 * 1024 * 1024; // 32 MB

use crate::util::fmt::{format_bytes, format_ms};

/// A complete in-RAM fetch: plaintext for the decoder, plus the ciphertext the disk
/// cache stores. `None` when staging failed or the fetch was capped - caching is
/// best-effort and must never fail a load.
pub struct FetchedTrack {
    pub data: Vec<u8>,
    pub ciphertext: Option<(tempfile::NamedTempFile, u64)>,
}

/// `owner` decides which bucket pays; this path has no streaming buffer to carry it, and a load
/// with no `Content-Length` falls back here too. `opened` is a response already in hand: issuing
/// a second GET holds the first open with its body unread, and an HTTP/1.1 connection in that
/// state is unusable until it drops.
async fn fetch_and_decrypt_inner(
    url: &str,
    key: &str,
    max_bytes: Option<usize>,
    owner: DownloadOwner,
    opened: Option<reqwest::Response>,
) -> anyhow::Result<Option<FetchedTrack>> {
    let start = std::time::Instant::now();
    let resp = match opened {
        Some(resp) => resp,
        None => HTTP_CLIENT_PRELOAD.get(url).send().await?,
    };

    if !resp.status().is_success() {
        anyhow::bail!("Upstream status: {}", resp.status());
    }

    // A cap counted only over what has arrived pays the whole ceiling before refusing:
    // the bytes are fetched and decrypted, then dropped. The announced size settles it
    // before the first byte of body. It is absent for a chunked or auto-decoded response,
    // and reqwest reports that as `None` rather than a smaller number, leaving the
    // running check below as the only guard when the size cannot be known up front.
    if let Some(limit) = max_bytes
        && let Some(announced) = resp.content_length()
        && announced > limit as u64
    {
        crate::vprintln!(
            "[PRELOAD] Skip RAM cache: announced {} > {}",
            format_bytes(announced),
            format_bytes(limit as u64)
        );
        return Ok(None);
    }

    let decryptor = if key.is_empty() {
        None
    } else {
        Some(FlacDecryptor::new(key)?)
    };
    let mut stream = resp.bytes_stream();
    let mut offset = 0u64;
    let mut decrypt_buf = Vec::new();
    let mut buffer = Vec::new();
    let mut reconnect_attempts: u32 = 0;
    const MAX_PRELOAD_RECONNECTS: u32 = 8;
    // A reconnect resumes at `offset` and appends; the staged bytes stay linear
    // from zero - no equivalent of download_stream's Range restart exists here.
    let mut cipher_sink = open_cipher_sink(&crate::player::canonical_track_id(url));

    'download: loop {
        let item = match stream.next().await {
            Some(item) => item,
            None => break 'download,
        };
        let chunk = match item {
            Ok(chunk) => chunk,
            Err(e) => {
                // Idle connection died on a long pause: reconnect at `offset` and keep
                // appending - the decrypt offset stays aligned and resumed bytes decrypt
                // correctly. A 416 means `offset` is at/past EOF (all bytes buffered): finish.
                crate::vprintln!("[PRELOAD] Stream error at byte {offset}: {e}");
                stream = 'reconnect: loop {
                    reconnect_attempts += 1;
                    if reconnect_attempts > MAX_PRELOAD_RECONNECTS {
                        anyhow::bail!(
                            "network error after {MAX_PRELOAD_RECONNECTS} reconnects: {e}"
                        );
                    }
                    let backoff = std::time::Duration::from_millis(250 * reconnect_attempts as u64);
                    tokio::time::sleep(backoff).await;
                    let range_header = format!("bytes={offset}-");
                    // On the owner's own pool, like the reconnects in `download_stream`. This
                    // path serves the active load as well as a preload now, and the two clients
                    // exist precisely to keep a listener's stream from queueing behind staged
                    // traffic; a reconnect that switched pools would hand it straight back.
                    match owner
                        .http_client()
                        .get(url)
                        .header("Range", &range_header)
                        .send()
                        .await
                    {
                        Ok(r) if r.status() == reqwest::StatusCode::PARTIAL_CONTENT => {
                            crate::vprintln!(
                                "[PRELOAD] Reconnected at byte {offset} (attempt {reconnect_attempts})"
                            );
                            break 'reconnect r.bytes_stream();
                        }
                        Ok(r) if r.status() == reqwest::StatusCode::RANGE_NOT_SATISFIABLE => {
                            break 'download;
                        }
                        Ok(r) => anyhow::bail!("reconnect status: {}", r.status()),
                        Err(send_err) => {
                            crate::vprintln!(
                                "[PRELOAD] reconnect attempt {reconnect_attempts} failed: {send_err}"
                            );
                            continue 'reconnect;
                        }
                    }
                };
                continue 'download; // new stream, buffer + offset intact
            }
        };

        reconnect_attempts = 0; // received data; reset the retry budget

        GOVERNOR
            .acquire(owner.traffic_class(), chunk.len() as u32)
            .await;

        // Stage the CDN's bytes before decrypting the copy below.
        if let Some(sink) = cipher_sink.take() {
            cipher_sink = sink.append(&chunk).await;
        }

        decrypt_buf.clear();
        decrypt_buf.extend_from_slice(&chunk);
        if let Some(ref dec) = decryptor {
            dec.decrypt_in_place(&mut decrypt_buf, offset)?;
        }
        offset += chunk.len() as u64;

        if let Some(limit) = max_bytes
            && buffer.len().saturating_add(decrypt_buf.len()) > limit
        {
            let elapsed = start.elapsed().as_secs_f64();
            crate::vprintln!(
                "[PRELOAD] Skip RAM cache: size > {} (received {} in {:.1}s)",
                format_bytes(limit as u64),
                format_bytes(offset),
                elapsed
            );
            return Ok(None);
        }

        buffer.extend_from_slice(&decrypt_buf);
    }

    let elapsed = start.elapsed().as_secs_f64();
    let rate_mbps = (offset as f64 * 8.0) / (elapsed * 1_000_000.0);
    crate::vprintln!(
        "[FETCH]  {:.1} MB in {:.1}s ({:.1} Mbps)",
        offset as f64 / 1_048_576.0,
        elapsed,
        rate_mbps
    );

    let ciphertext = match cipher_sink {
        Some(sink) => sink.finish().await,
        None => None,
    };

    Ok(Some(FetchedTrack {
        data: buffer,
        ciphertext,
    }))
}

/// The uncapped whole-copy fetch, for the load path that has no `Content-Length` to build a
/// streaming buffer from. Its bytes are the listener's: the caller is waiting to hear them,
/// which is also why it hands over the response it already opened rather than leaving it to
/// idle against a second one.
pub async fn fetch_and_decrypt(
    url: &str,
    key: &str,
    opened: reqwest::Response,
) -> anyhow::Result<FetchedTrack> {
    match fetch_and_decrypt_inner(url, key, None, DownloadOwner::Playback, Some(opened)).await? {
        Some(fetched) => Ok(fetched),
        None => anyhow::bail!("unexpected capped fetch in uncapped mode"),
    }
}

/// Enough of the head that a probe cannot reach for bytes that have not arrived: `arm_crossfade`
/// probes on the PLAYER thread, where a read that waits costs up to thirty seconds of frozen
/// transport. The load path's own measured figure, and not sized to a whole fade: a larger
/// "safety" margin threw away heads that were nearly there (188 KB arriving where 256 KB was
/// demanded), and arming clamps the fade to whatever landed anyway.
const HEAD_TARGET_BYTES: u64 = 64 * 1024;
/// Generous on purpose, and nothing waits on it: this runs in its own task, seconds
/// before any fade needs the bytes. Two seconds came from the load path, where a
/// listener IS waiting to hear audio; the governor's gate can hold the head longer.
const HEAD_WAIT_TIMEOUT_MS: u64 = 8000;

/// The staged "next" track and the load that named it, as one value: read separately, a track
/// and a generation can be paired stale with fresh. The stamp stays out of `TrackInfo`, whose
/// hand-written `PartialEq` answers stream identity and must not gain a field to exclude.
pub struct StagedNext {
    track: TrackInfo,
    load_gen: u32,
}

/// Every field is private and every transition is a method below, because every defect this
/// shape prevents came from a site moving PART of the group: a guard that re-affirmed the staged
/// next without re-stamping its generation, two branches that dropped a record's name while
/// leaving its download alive. No method moves one half of a pair.
pub struct PreloadState {
    next: Option<StagedNext>,
    data: Option<PreloadedTrack>,
    /// The staging task and the download it spawned. `task` covers both fetch shapes;
    /// `download_cancel` exists only for the streaming one, and only once its response has
    /// been seen: a live `task` with no `download_cancel` is an ordinary state, not a gap.
    /// That is why disposal has to name both: aborting the task leaves the download running,
    /// because `start_download` runs it in a task the staging one does not own.
    task: Option<tokio::task::JoinHandle<()>>,
    download_cancel: Option<CancellationToken>,
    /// Which staging attempt owns `task`, `download_cancel` and `data`.
    ///
    /// `start_preload` cannot hold this lock across its awaits, so the decision to stage and the
    /// handles it produces land in separate critical sections. Unnamed in between, two concurrent
    /// callers overwrite each other's handles and the survivor can name a different track than
    /// `next`, defeating cancel-on-skip.
    attempt: u64,
}

impl PreloadState {
    const fn new() -> Self {
        Self {
            next: None,
            data: None,
            task: None,
            download_cancel: None,
            attempt: 0,
        }
    }

    /// Claim the right to stage `track`, or refuse because a live attempt already covers it.
    ///
    /// Refusing and re-stamping are ONE operation on purpose: a bare early return left the
    /// generation stamped by whichever load first named the track, so readers rejected a
    /// perfectly good staged copy and the same guard refused every call that would have healed
    /// it. Keyed on the canonical id, the signed url differing between two requests for one
    /// track.
    fn claim(&mut self, track: &TrackInfo, load_gen: u32) -> Option<u64> {
        let id = crate::player::canonical_track_id(&track.url);
        let already_named = self
            .next
            .as_ref()
            .is_some_and(|next| crate::player::canonical_track_id(&next.track.url) == id);
        // Occupancy stood in for liveness, and the two part company the moment a download dies
        // AFTER its head landed: `data` stays full while `task` is already finished, since
        // `stage_streaming` returns as soon as the head arrives and the transfer running past
        // that point belongs to a task this field never named. The corpse then read as a live
        // attempt on every later ask. Only the buffer knows its own download died.
        let still_working = self
            .data
            .as_ref()
            .is_some_and(|staged| !staged.buffer.has_failed())
            || self.task.as_ref().is_some_and(|task| !task.is_finished());
        if already_named && still_working {
            if let Some(next) = self.next.as_mut() {
                next.load_gen = load_gen;
            }
            return None;
        }
        self.reset();
        self.next = Some(StagedNext {
            track: track.clone(),
            load_gen,
        });
        Some(self.attempt)
    }

    /// Stop the current attempt and make sure nothing it spawned can still land.
    ///
    /// The bump is the half that is easy to forget: retiring the handles stops the work that
    /// has started, while renaming the attempt is what stops the work already in flight from
    /// publishing into a slot its owner no longer holds.
    fn reset(&mut self) {
        self.retire_attempt();
        self.attempt += 1;
    }

    /// Hand the staging task's handle over, if the attempt that spawned it still owns the slot.
    ///
    /// A handle from a superseded attempt is aborted rather than stored: stored, it would
    /// answer `still_working` for a track nobody is staging, and a later disposal would abort
    /// it while the attempt actually in flight ran on unreachable.
    fn install_task(&mut self, attempt: u64, handle: tokio::task::JoinHandle<()>) {
        if attempt == self.attempt {
            self.task = Some(handle);
        } else {
            handle.abort();
        }
    }

    /// Publish the token that stops this attempt's download. False means the attempt was
    /// superseded while its response was in flight and the caller must cancel its own copy.
    fn arm_download(&mut self, attempt: u64, cancel: CancellationToken) -> bool {
        if attempt != self.attempt {
            return false;
        }
        self.download_cancel = Some(cancel);
        true
    }

    /// Publish the staged bytes, if this attempt still owns the slot.
    ///
    /// Gated on the ATTEMPT, not on `next` still naming the track: a load of the very track being
    /// staged clears the name on its way past, and a gate reading `next` refused the bytes that
    /// load was about to ask for, so the ordinary path downloaded the whole track a second time.
    fn publish(
        &mut self,
        attempt: u64,
        track: &TrackInfo,
        buffer: crate::player::buffer::RamBuffer,
    ) -> bool {
        if attempt != self.attempt {
            return false;
        }
        self.data = Some(PreloadedTrack {
            track: track.clone(),
            buffer,
        });
        true
    }

    /// Take back a slot this attempt published but could not fill.
    ///
    /// A buffer left standing after its own download was stopped is worse than no buffer: a load
    /// would adopt it and wait on bytes that can never come. `next` survives, the track still
    /// being the one that comes after this one. Only the slot goes: `download_cancel` is spent
    /// rather than stale, and erasing it would erase the one record of a download to stop.
    fn abandon(&mut self, attempt: u64) {
        if attempt != self.attempt {
            return;
        }
        self.data = None;
    }

    /// Stop and forget whatever the current attempt owns, parking any complete ciphertext.
    ///
    /// The one place disposal is written, because three callers cannot each get a different
    /// subset of it right. A staged track the listener never reached still cost a download and a
    /// decrypt, so complete bytes go to the disk cache before the record does; a partial file
    /// indexed as whole would later be served as valid.
    fn retire_attempt(&mut self) {
        if let Some(handle) = self.task.take() {
            handle.abort();
        }
        if let Some(cancel) = self.download_cancel.take() {
            cancel.cancel();
        }
        if let Some(staged) = self.data.as_ref()
            && staged.buffer.is_complete()
            && let Some((file, len)) = staged.buffer.take_ciphertext()
        {
            crate::player::cache::AudioCache::store_ciphertext_detached(
                crate::player::canonical_track_id(&staged.track.url),
                staged.track.format.clone(),
                file,
                len,
            );
        }
        self.data = None;
        self.next = None;
    }

    /// True while the staged "next" still answers the load that is current.
    ///
    /// A staged next belongs to the load that asked for it and to no other. Every reader comes
    /// through here rather than comparing at its own call site, so the rule has one home.
    /// Repeat-one passes by construction, re-staging the current track under the SAME load;
    /// what fails is a record left by a load since superseded, the only case anyone wants
    /// refused.
    ///
    /// The stamp alone dates a record without naming whose queue it came from. This slot has
    /// one producer (`start_preload`, called only from the renderer's `player.preload` IPC),
    /// and the renderer's queue is not synchronised with a controller's: a preload staged after
    /// another origin took over carries that origin's generation and would read as fresh.
    ///
    /// Deliberately silent on whether a receiver is running or a phone is attached: a phone
    /// attached but not casting leaves the renderer's own load current, and local gapless and
    /// crossfade go on working as they always did.
    fn next_is_current(&self) -> bool {
        let (cur_gen, origin) = crate::player::current_load();
        self.next.as_ref().is_some_and(|next| {
            next.load_gen == cur_gen && origin == crate::player::LoadOrigin::Local
        })
    }

    /// Hands back the generation the record was validated against, not just the track. Returning
    /// the track alone left the caller to sample the counter a second time for the other half of
    /// a pair only this check has proven coherent.
    fn peek_next(&self) -> Option<RetainedTrack> {
        self.next_is_current()
            .then(|| {
                self.next.as_ref().map(|next| RetainedTrack {
                    track: next.track.clone(),
                    load_gen: next.load_gen,
                })
            })
            .flatten()
    }

    /// Take the staged next for the completion path, dropping a superseded record.
    ///
    /// Dropping it retires the whole attempt, not just the name. Clearing the name alone left
    /// the download that fed it running uncapped, competing for bandwidth with the track
    /// playing and holding its buffer until some unrelated later preload swept it.
    fn take_next(&mut self) -> Option<TrackInfo> {
        if !self.next_is_current() {
            if self.next.is_some() {
                crate::vprintln!(
                    "[PRELOAD] Staged next track belongs to a superseded load, dropping"
                );
                self.reset();
            }
            return None;
        }
        self.next.take().map(|next| next.track)
    }

    fn peek_data(&self) -> Option<PeekedTrack> {
        let staged = self.data.as_ref()?;
        // Failure before bytes, the order `RamBuffer::read` itself judges by: a dead download
        // refuses its very next read however much already arrived; clearing the head target
        // below proves nothing about a corpse. That is why this is asked first.
        if staged.buffer.has_failed() {
            return None;
        }
        // Published is not the same as usable, and this is the one reader where it bites: the
        // slot fills as soon as bytes arrive, but `arm_crossfade` probes on the PLAYER thread
        // with a BLOCKING read and parks for up to thirty seconds if the head has not landed.
        // Charged to the only caller needing it, and arming polls four times a second, so
        // refusing costs one tick.
        if staged.buffer.written() < HEAD_TARGET_BYTES.min(staged.buffer.total_len()) {
            return None;
        }
        Some(PeekedTrack {
            track: staged.track.clone(),
            // An `Arc` bump over the same bytes, not a copy: refusing costs nothing and the
            // ciphertext stays where it is. A refused track still reaches the disk cache by
            // the ordinary path.
            buffer: staged.buffer.clone(),
        })
    }

    /// Spend the record for a track that has just become current.
    ///
    /// Both halves key on the canonical id. Full equality reads as stricter and is wrong here,
    /// the signed url being a credential rather than an identity: a copy re-staged under a fresh
    /// url left the name standing while its bytes were spent. A different next track has a
    /// different canonical id and is still left alone.
    fn commit(&mut self, committed: &TrackInfo) {
        let committed_id = crate::player::canonical_track_id(&committed.url);
        if self.data.as_ref().is_some_and(|staged| {
            crate::player::canonical_track_id(&staged.track.url) == committed_id
        }) {
            // Taken rather than cleared, because the buffer has to be in hand to hand its
            // download over. Its bytes are the listener's from here: they are charged to the
            // playback bucket, where the fixed preload rate sits below what a hi-res track
            // needs to stay fed, and a reconnect reads the credential `CURRENT_TRACK` carries.
            if let Some(staged) = self.data.take() {
                staged.buffer.adopt_as_playback();
            }
            // The download feeding those bytes is no longer ours to stop. A staged buffer is
            // published while it is still filling; the track being committed here is playing
            // from a download that must run to the end of the file: cancelling it on the next
            // preload kills the audio of the track now playing, two minutes in. Clearing this
            // copy strands nothing: the buffer carries the same token.
            self.download_cancel = None;
        }
        if self
            .next
            .as_ref()
            .is_some_and(|next| crate::player::canonical_track_id(&next.track.url) == committed_id)
        {
            self.next = None;
        }
    }

    /// The staged bytes were handed to a decoder and it gave up on them.
    ///
    /// `next` survives on purpose: it names the track the hard cut is about to advance to, and
    /// that cut is the whole answer to a fade that could not run. Strict equality, where `commit`
    /// takes the canonical id, because this is a consumer discarding a record it knows is bad: a
    /// copy re-staged under a fresh url is untried and has to stand. The bytes are not parked to
    /// the disk cache either, a copy a decoder rejected failing the same way from disk.
    fn discard_failed(&mut self, failed: &TrackInfo) {
        if self
            .data
            .as_ref()
            .is_some_and(|staged| &staged.track == failed)
        {
            self.data = None;
            // Nothing reads another byte of it. Left running, the task also keeps
            // `still_working` true, which suppresses a legitimate re-preload of the same
            // track later.
            if let Some(cancel) = self.download_cancel.take() {
                cancel.cancel();
            }
        }
    }

    /// A load for `track_id` has begun, and that track is no longer the NEXT one, unless the
    /// record naming it is one the current load can still use.
    ///
    /// Freshness is NOT re-derived here: `next_is_current` owns that rule, and a second
    /// predicate beside it is how two halves of one rule drift apart. This site keyed on the
    /// canonical id alone while its own doc claimed it weighed the moment, destroying a record
    /// staged FOR the load now asking, which only a deliberate repeat-one produces.
    ///
    /// Asking the shared rule rather than the caller's own generation is what makes a third
    /// load safe: an older load's task, running late, must not take a record belonging to the
    /// load that superseded it. "Newer than mine" would also have to order a counter that wraps.
    ///
    /// Only the name goes. The bytes stay for the load that just began to ask for them, and the
    /// attempt keeps its claim so a staging task still in flight publishes into that same
    /// request rather than being fetched twice.
    fn clear_next_for_load(&mut self, track_id: &str) {
        let names_the_load = self
            .next
            .as_ref()
            .is_some_and(|next| crate::player::canonical_track_id(&next.track.url) == track_id);
        if names_the_load && !self.next_is_current() {
            self.next = None;
        }
    }

    fn take_data_if_match(&mut self, track: &TrackInfo) -> Option<PreloadedTrack> {
        // Health weighed alongside identity, never after it. Handing a dead download to a load
        // costs more than refusing it does: the refusal buys an ordinary fetch, whereas the
        // handover buys a read error once the track is already playing. Refused, the copy is
        // left where it sits. The load that follows finds the slot exactly as a name
        // mismatch would have left it.
        if !self
            .data
            .as_ref()
            .is_some_and(|staged| staged.track == *track && !staged.buffer.has_failed())
        {
            return None;
        }
        self.next = None;
        // Handed over with the buffer, for the same reason as in `commit`: the caller is about
        // to play these bytes and the rest of them are still arriving. Clearing this copy
        // strands nothing: the buffer carries the same token.
        self.download_cancel = None;
        let taken = self.data.take();
        if let Some(staged) = taken.as_ref() {
            staged.buffer.adopt_as_playback();
        }
        taken
    }
}

/// The narrow window other modules' tests get onto this state.
///
/// Two accessors that cannot express a partial update are the whole surface: a test able to set
/// a name without its stamp could build a state the production code can no longer reach.
#[cfg(test)]
impl PreloadState {
    pub(crate) fn stage_for_test(&mut self, track: TrackInfo) {
        self.next = Some(StagedNext {
            track,
            load_gen: crate::player::current_gen(),
        });
    }

    pub(crate) fn staged_next(&self) -> Option<&TrackInfo> {
        self.next.as_ref().map(|next| &next.track)
    }
}

/// Deliberately a std mutex, not a tokio one: no holder suspends while holding it, and the
/// player thread (which has no runtime) has to take it unconditionally at a promotion. A tokio
/// mutex offers that thread only `blocking_lock`, which panics inside a runtime, or a
/// try-and-skip that turns a one-shot commit into a silent no-op.
pub static PRELOAD_STATE: std::sync::Mutex<PreloadState> =
    std::sync::Mutex::new(PreloadState::new());

fn preload_state() -> std::sync::MutexGuard<'static, PreloadState> {
    PRELOAD_STATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Stage the next track as a stream: publish the buffer while it is still filling, the
/// way the ordinary load path plays a track while downloading it.
///
/// This removes the size ceiling rather than raising it. Holding a whole track in RAM first
/// meant 32 MB, two minutes forty-three of CD FLAC: the fade only worked on a cached library.
async fn stage_streaming(track: &TrackInfo, attempt: u64) -> anyhow::Result<()> {
    let resp = HTTP_CLIENT_PRELOAD.get(&track.url).send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("Upstream status: {}", resp.status());
    }

    let total_len = resp.content_length().unwrap_or(0);
    if total_len == 0 {
        // A streaming buffer needs a total length; without one the only option is the
        // whole-copy fetch, the single place a ceiling still earns its keep because nothing
        // else bounds a body of unknown size. The response already in hand goes with it:
        // leaving it to issue a second GET held this one open with its body unread, and on
        // HTTP/1.1 that connection is neither drained nor reusable until it drops.
        let Some(fetched) = fetch_and_decrypt_inner(
            &track.url,
            &track.key,
            Some(PRELOAD_MAX_BYTES),
            DownloadOwner::Preload,
            Some(resp),
        )
        .await?
        else {
            return Ok(());
        };
        if fetched.data.is_empty() {
            return Ok(());
        }
        // Built once, here: the ciphertext lives inside the buffer's shared state from the
        // start and survives any number of cheap reader clones.
        preload_state().publish(
            attempt,
            track,
            crate::player::buffer::RamBuffer::from_complete_with_ciphertext(
                fetched.data,
                fetched.ciphertext,
            ),
        );
        return Ok(());
    }

    let cancel = CancellationToken::new();
    let (buffer, writer) =
        crate::player::buffer::RamBuffer::new(total_len, DownloadOwner::Preload, cancel.clone());
    // A clone kept for the window before the buffer is published: until then no other holder
    // exists, and a disposal has to reach this download somehow. Clearing it at adoption is
    // safe because the buffer carries the same token. Refused means this attempt lost its claim
    // while the response was in flight, before `start_download`, so there is no download left
    // running.
    if !preload_state().arm_download(attempt, cancel.clone()) {
        return Ok(());
    }
    start_download(resp, track.url.clone(), track.key.clone(), writer);

    // Published NOW, while the buffer is still empty, so a load for this same track adopts the
    // download already running instead of opening a second one. Publishing only after the head
    // landed left the whole head wait, up to eight seconds, where the one reader that could
    // reuse this work could not see it and fetched the file again in parallel. Readers that
    // cannot take an unfilled buffer are held back by `peek_data`'s own head check. Refused
    // means another attempt has already retired this one: nothing left to undo.
    if !preload_state().publish(attempt, track, buffer.clone()) {
        return Ok(());
    }

    // Awaited HERE, in this async task, never on the player thread. A probe of a buffer
    // whose head has not arrived can stall that thread for thirty seconds.
    let head_target = HEAD_TARGET_BYTES.min(total_len);
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_millis(HEAD_WAIT_TIMEOUT_MS);
    loop {
        match buffer.head_status(head_target) {
            HeadStatus::Landed => break,
            // Its own exit, deliberately not the cancelled one below. A cancelled token
            // means some other holder has already retired this attempt, which is why that
            // exit leaves the slot alone. Nothing retired anything here (the download
            // ended by itself), and this task owes the cleanup the deadline owes.
            HeadStatus::Ended(cause) => {
                // Stops no download; this one is over. It keeps `abandon`'s precondition
                // true, which reads this token as spent rather than as still naming
                // something to stop.
                cancel.cancel();
                preload_state().abandon(attempt);
                match cause {
                    Some(cause) => crate::vprintln!(
                        "[PRELOAD] Download died at {} of {}, not staging: {cause}",
                        format_bytes(buffer.written()),
                        format_bytes(head_target)
                    ),
                    None => crate::vprintln!(
                        "[PRELOAD] Download ended short at {} of {}, not staging",
                        format_bytes(buffer.written()),
                        format_bytes(head_target)
                    ),
                }
                return Ok(());
            }
            HeadStatus::Filling => {}
        }
        if cancel.is_cancelled() {
            return Ok(());
        }
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            // Staging a buffer whose head has not arrived hands `arm_crossfade` a probe
            // that reads past what exists, on the player thread, for up to thirty seconds
            // of frozen transport. Nothing staged means a hard cut, the fallback this
            // feature already has. The download is stopped rather than left to finish:
            // uncapped, it would accumulate the whole track and compete for bandwidth with
            // the one being listened to, and none of it is recoverable anyway.
            cancel.cancel();
            // And the slot has to go back, because it was published before this wait. A
            // buffer left standing after its download was stopped is worse than none: a load
            // would adopt it and wait on bytes that can no longer come. `next` survives: the
            // track is still the one after this, the completion path just loads it the
            // ordinary way, which is the hard cut this feature already falls back to.
            preload_state().abandon(attempt);
            crate::vprintln!(
                "[PRELOAD] Head did not arrive in {}ms ({} of {}), not staging",
                HEAD_WAIT_TIMEOUT_MS,
                format_bytes(buffer.written()),
                format_bytes(head_target)
            );
            return Ok(());
        }
        let _ = tokio::time::timeout(left, buffer.notified()).await;
    }

    crate::vprintln!(
        "[PRELOAD] Staged streaming ({} of {} after {}ms)",
        format_bytes(buffer.written()),
        format_bytes(total_len),
        HEAD_WAIT_TIMEOUT_MS.saturating_sub(
            deadline
                .saturating_duration_since(std::time::Instant::now())
                .as_millis() as u64
        )
    );
    Ok(())
}

pub async fn start_preload(track: TrackInfo) {
    // Asking twice for the same track is a no-op, not a restart. The one producer re-enters
    // itself: the SDK's `next()` clears its re-entrancy flag through a call that never
    // suspends, awaits it anyway, and resumes without revalidating. A second call landing in
    // that window reads the flag as clear, skips the cancel it would otherwise send us, and
    // preloads again while the first is outstanding. Restarting would cancel the first
    // download and begin from zero, throwing away the head start already bought.
    //
    // The check, the disposal of whatever it supersedes, and the claim are ONE critical
    // section. Split into three, two concurrent callers each pass the check before either
    // writes, and the surviving handles can name a different track than the record does, which
    // is how cancel-on-skip stops reaching the download it means to.
    //
    // Stamped with the load this "next" answers to, read here rather than passed in: the
    // renderer names the track off its own queue, whose load is whatever is current then.
    let Some(attempt) = preload_state().claim(&track, crate::player::current_gen()) else {
        return;
    };

    let handle = tokio::spawn(async move {
        if track.url.is_empty() {
            return;
        }

        // try_cache_hit serves a cached track before the preload is consulted; fetching
        // it would spend network and a staged copy on bytes nothing reads. next_track
        // stays set: the ordinary load path serves this one from disk.
        let already_cached = crate::state::AUDIO_CACHE.lock().ok().is_some_and(|c| {
            c.lookup_path(&crate::player::canonical_track_id(&track.url))
                .is_some()
        });
        if already_cached {
            crate::vprintln!("[PRELOAD] Next track is already cached, skipping fetch");
            return;
        }

        crate::vprintln!("[PRELOAD] Starting preload for next track");
        if let Err(e) = stage_streaming(&track, attempt).await {
            crate::vprintln!("[PRELOAD] Failed: {}", e);
        }
    });

    preload_state().install_task(attempt, handle);
}

/// Drop the staged record and stop everything feeding it.
pub async fn cancel_preload() {
    preload_state().reset();
}

/// Return the next track info (set during preload) without consuming preloaded data.
/// Used by the auto-load logic after "completed" to know which track to load next.
///
/// A record from a superseded load is dropped rather than returned: it named a queue nobody is
/// playing, and promoting it starts a track the listener never chose.
pub async fn take_next_track() -> Option<TrackInfo> {
    preload_state().take_next()
}

/// What a caller needs to decide whether it can use the staged track, WITHOUT
/// consuming it. The buffer is a cheap clone over shared bytes.
pub struct PeekedTrack {
    pub track: TrackInfo,
    pub buffer: crate::player::buffer::RamBuffer,
}

/// Take the lock if it is free. For the callers a miss costs nothing: they poll again.
///
/// A poisoned lock is recovered rather than read as busy: std poisons where tokio did not,
/// and mapping a poison onto "come back later" would wedge these callers for the rest of the
/// process instead of for one tick.
fn try_preload_state() -> Option<std::sync::MutexGuard<'static, PreloadState>> {
    match PRELOAD_STATE.try_lock() {
        Ok(lock) => Some(lock),
        Err(std::sync::TryLockError::Poisoned(e)) => Some(e.into_inner()),
        Err(std::sync::TryLockError::WouldBlock) => None,
    }
}

/// The track staged as "next" without consuming it, or `None` if none is staged
/// or the lock is held right now.
///
/// Distinct from [`peek_preloaded`]: `next_track` is set even when no bytes were staged, which
/// is what happens for a track already in the disk cache. A record from a superseded load reads
/// as nothing, arming a fade into it playing a track off a queue nobody is on. The generation
/// travels with the track because the two are only coherent together.
pub fn peek_next_track() -> Option<RetainedTrack> {
    try_preload_state()?.peek_next()
}

/// Look at the staged track without taking it, or `None` if nothing is staged or
/// the lock is held right now.
///
/// Skipping on a busy lock is affordable HERE, and only here: arming polls four times a second,
/// so a miss costs one tick. Peek and commit are separate because arming can still refuse the
/// track after inspecting it, and consuming first destroyed the staged track on every refusal,
/// leaving the completion path with nothing to advance to.
pub fn peek_preloaded() -> Option<PeekedTrack> {
    try_preload_state()?.peek_data()
}

/// Consume the track a previous [`peek_preloaded`] inspected, once the caller has
/// committed to playing it.
///
/// Both halves key on the canonical id; see [`PreloadState::commit`] for why the name cannot
/// key on the signed url.
pub fn commit_peeked(committed: &TrackInfo) {
    // Unconditional, unlike the peeks above: this runs once per promotion and never again.
    // Skipping it on a busy lock leaves `download_cancel` naming the track that just BECAME
    // current, and the next ordinary preload cancels it, killing the download of the track
    // being listened to. The wait is bounded by the longest critical section here, which is a
    // handful of field writes: no holder of this lock suspends while holding it.
    preload_state().commit(committed);
}

/// The staged bytes were handed to a decoder and it gave up on them. Drop the copy and
/// stop the download still filling it.
///
/// `next_track` survives: it names the track the hard cut is about to advance to. The failed
/// copy must not, because re-armed it fails the same way on every poll and the completion path
/// hands it to the load as a preload hit, so the cut meant to rescue the transition lands on the
/// same unusable bytes. Strict equality: see [`PreloadState::discard_failed`].
pub fn discard_staged_if_match(failed: &TrackInfo) {
    // Unconditional for the same reason as `commit_peeked`: the branch that calls this fires
    // once, when the incoming decoder's failure is drained, and cannot fire again once the
    // fade is cancelled. A miss here is permanent, and it leaves the copy that already failed
    // for the hard cut to fail on again.
    preload_state().discard_failed(failed);
}

/// A load for `track_id` has begun. That track is no longer the NEXT one: drop
/// the staged record if it names it.
///
/// The two clears above need staged bytes or a committed fade, and a cache-hit load returns
/// before either runs. `next_track` then keeps naming the track now playing, and the readers
/// `commit_peeked` warns about act on it: the crossfade arms a fade of the track into itself,
/// and the completion path reloads it instead of advancing.
///
/// Staleness stays [`PreloadState::next_is_current`]'s business rather than a second copy here,
/// so a repeat-one preload naming the current track is left standing for the completion path to
/// replay.
///
/// Keyed on the canonical id, not on `TrackInfo`, the signed url being a credential that differs
/// between a preload and the load naming the same track: a url compare would silently never
/// match. A genuinely different next track is left alone. `data` is untouched: a cache-hit load
/// staged none, and a staged copy still belongs to whoever loads it.
pub async fn clear_next_track_if_match(track_id: &str) {
    preload_state().clear_next_for_load(track_id);
}

pub async fn take_preloaded_if_match(track: &TrackInfo) -> Option<PreloadedTrack> {
    preload_state().take_data_if_match(track)
}

/// Wait until an adopted buffer holds enough to be probed, and report whether it got there.
///
/// Awaited HERE, in the async load task, for the reason `stage_streaming` waits in its own:
/// **never on the player thread**. That thread probes with a BLOCKING read, and handed a buffer
/// whose head has not arrived it parks for up to thirty seconds with the transport frozen: no
/// time updates, no seek, no pause. `peek_data` guards `arm_crossfade` against exactly this;
/// the load path is the other consumer, and it had no guard at all.
///
/// Waiting rather than refusing, because a refusal sends the load down the ordinary path and
/// opens a SECOND download of bytes already arriving. Only the handover to the player thread
/// waits: the adoption still happens the instant the record is taken.
///
/// That trade buys nothing once bytes can no longer come, so `HeadStatus::Ended` refuses at
/// once rather than spending the deadline in silence with the outgoing track already stopped.
/// `still_wanted` is the caller's own claim: a wait nobody is waiting for ends here.
pub async fn head_has_landed(
    buffer: &crate::player::buffer::RamBuffer,
    still_wanted: impl Fn() -> bool,
) -> bool {
    let head_target = HEAD_TARGET_BYTES.min(buffer.total_len());
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_millis(HEAD_WAIT_TIMEOUT_MS);
    loop {
        match buffer.head_status(head_target) {
            HeadStatus::Landed => return true,
            // Named rather than folded into the timeout log below: four of the six failure
            // sites report at `LOGS=3` and two report nothing; this is where a listener
            // losing a preload to a dead download can see why.
            HeadStatus::Ended(Some(cause)) => {
                crate::vprintln!(
                    "[PRELOAD] Adopted download died at {} of {}: {cause}",
                    format_bytes(buffer.written()),
                    format_bytes(head_target)
                );
                return false;
            }
            HeadStatus::Ended(None) => {
                crate::vprintln!(
                    "[PRELOAD] Adopted download ended short, {} of {}",
                    format_bytes(buffer.written()),
                    format_bytes(head_target)
                );
                return false;
            }
            HeadStatus::Filling => {}
        }
        if !still_wanted() {
            return false;
        }
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            crate::vprintln!(
                "[PRELOAD] Head never landed for this load ({} of {})",
                format_bytes(buffer.written()),
                format_bytes(head_target)
            );
            return false;
        }
        let _ = tokio::time::timeout(left, buffer.notified()).await;
    }
}

/// Start a streaming download into a RamBufferWriter.
/// Handles decryption, governor rate limiting, and Range restarts.
pub fn start_download(
    resp: reqwest::Response,
    url: String,
    key: String,
    writer: RamBufferWriter,
) -> tokio::task::JoinHandle<()> {
    // Taken from the buffer rather than passed in: the buffer is what gets handed from a
    // staged track to the one being listened to, and the token has to travel with it.
    let cancel = writer.cancel_token();
    // On cancel, dropping the inner future drops the stream + writer, which closes
    // the HTTP connection and frees the buffer at once.
    tokio::spawn(async move {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {}
            _ = download_stream(resp, url, key, writer) => {}
        }
    })
}

/// How much ciphertext to accumulate before touching the disk.
const STAGING_FLUSH_BYTES: usize = 1024 * 1024;

/// Collects the stream's bytes as they arrive from the CDN, before decryption, so
/// the disk cache stores ciphertext rather than playable audio. Chunks pile up in
/// RAM and reach the disk one `STAGING_FLUSH_BYTES` block at a time on a blocking
/// worker: reqwest yields 16-64 KB at a time, and writing each inline would put
/// thousands of blocking writes on the runtime thread driving the download.
struct CipherSink {
    file: tempfile::NamedTempFile,
    len: u64,
    pending: Vec<u8>,
}

impl CipherSink {
    /// By value because a flush moves the file onto a blocking worker. `None` means
    /// staging failed and the caller must stop staging.
    async fn append(mut self, chunk: &[u8]) -> Option<Self> {
        self.pending.extend_from_slice(chunk);
        if self.pending.len() < STAGING_FLUSH_BYTES {
            return Some(self);
        }
        self.flush().await
    }

    async fn flush(mut self) -> Option<Self> {
        if self.pending.is_empty() {
            return Some(self);
        }
        tokio::task::spawn_blocking(move || {
            use std::io::Write;
            self.file.write_all(&self.pending).ok()?;
            self.len += self.pending.len() as u64;
            self.pending.clear();
            Some(self)
        })
        .await
        .ok()?
    }

    /// Flush the tail and hand over the staging file, or `None` if nothing was ever
    /// written. The one gate on emptiness, sparing every caller its own: a zero-length
    /// entry would be stored, read back, rejected and deleted for nothing.
    async fn finish(self) -> Option<(tempfile::NamedTempFile, u64)> {
        let sink = self.flush().await?;
        (sink.len > 0).then_some((sink.file, sink.len))
    }
}

/// Staging sink for the ciphertext, or `None` if this track should not be staged.
/// Caching is best-effort: every failure here disables it silently. Refuses an
/// already-indexed track and a disabled cache: neither could use the result, and
/// both would write a full track only to delete it.
fn open_cipher_sink(track_id: &str) -> Option<CipherSink> {
    let dir = {
        let cache = crate::state::AUDIO_CACHE.lock().ok()?;
        if cache.lookup_path(track_id).is_some() {
            return None;
        }
        cache.staging_dir()?
    };
    // Unlocked: syscalls, and the cache lock sits on every concurrent lookup.
    std::fs::create_dir_all(&dir).ok()?;
    let file = tempfile::NamedTempFile::new_in(&dir).ok()?;
    Some(CipherSink {
        file,
        len: 0,
        pending: Vec::with_capacity(STAGING_FLUSH_BYTES),
    })
}

/// Hand a complete-from-zero download's ciphertext to the cache writer. Every EOF
/// path goes through here because one that forgets silently stops caching instead
/// of failing, which is how the 416-on-reconnect exit was missed. A Range-started
/// stream is not cacheable (the gate is `base_offset == 0`), and it drops the sink.
async fn park_ciphertext(writer: &RamBufferWriter, sink: Option<CipherSink>, stream_offset: u64) {
    if stream_offset != 0 {
        return;
    }
    if let Some(sink) = sink
        && let Some((file, len)) = sink.finish().await
    {
        writer.set_ciphertext(file, len);
    }
}

/// The credential this task must fetch with now, or `None` if the task is stale. Read at each
/// re-fetch rather than captured at start: the signed url is refreshed in place on a same-track
/// re-assert; a captured copy would otherwise expire while the task is still legitimately running.
fn current_fetch_url(task_canonical_id: &str) -> Option<String> {
    let retained = crate::state::CURRENT_TRACK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    crate::player::refreshed_fetch_url(task_canonical_id, retained.as_ref().map(|r| &r.track))
}

/// What a download's current owner settles: which queue governs its bytes, which pool it
/// reconnects on, and where it finds a credential to reconnect with.
///
/// The two owners differ in one fact, and everything here follows from it: whether the track
/// being fetched is the one `CURRENT_TRACK` names.
impl DownloadOwner {
    fn traffic_class(self) -> TrafficClass {
        match self {
            Self::Playback => TrafficClass::Playback,
            Self::Preload => TrafficClass::Preload,
        }
    }

    /// A staged download reconnects on the pool it started on. The two clients exist to
    /// keep preload traffic off the playback pool's contention, and a reconnect that
    /// switched pools would hand it straight back.
    fn http_client(self) -> &'static reqwest::Client {
        match self {
            Self::Playback => &crate::state::HTTP_CLIENT_PLAYBACK,
            Self::Preload => &HTTP_CLIENT_PRELOAD,
        }
    }

    /// The credential to reconnect with, or `None` when the task is stale and must stop.
    ///
    /// A staged download uses the url captured at spawn, re-staging going through
    /// `cancel_preload` so a fresh url arrives with a fresh task. Once adopted, the track IS
    /// current and a same-track re-assert can re-sign this task's url underneath it, which is
    /// when the global becomes the right place to read.
    fn fetch_url(self, task_canonical_id: &str, captured: &str) -> Option<String> {
        match self {
            Self::Playback => current_fetch_url(task_canonical_id),
            Self::Preload => Some(captured.to_string()),
        }
    }
}

async fn download_stream(
    resp: reqwest::Response,
    url: String,
    key: String,
    writer: RamBufferWriter,
) {
    // Captured once: it names which track this task belongs to, and for a staged download
    // it is also the credential every reconnect uses. A playback download re-reads its own
    // through `owner.fetch_url`, since that one can be refreshed under us.
    let task_canonical_id = crate::player::canonical_track_id(&url);
    let download_start = std::time::Instant::now();
    let decryptor = if key.is_empty() {
        None
    } else {
        match FlacDecryptor::new(&key) {
            Ok(d) => Some(d),
            Err(e) => {
                writer.finish_with_error(
                    DownloadFailure::Source,
                    format!("decrypt init failed: {e}"),
                );
                return;
            }
        }
    };

    let mut current_resp = Some(resp);
    let mut range_restarts: u32 = 0;
    let mut http_requests: u32 = 1; // initial GET counts as 1
    // Consecutive reconnect attempts after a mid-stream transport error,
    // reset on the next successful chunk. Bounds a truly-dead network.
    let mut reconnect_attempts: u32 = 0;
    const MAX_RECONNECTS: u32 = 8;

    'outer: loop {
        let (stream_resp, stream_offset) = if let Some(r) = current_resp.take() {
            (r, 0u64)
        } else {
            // Range restart requested
            let restart_t0 = std::time::Instant::now();
            let target = match writer.take_restart_target() {
                Some(t) => t,
                None => break,
            };
            // A seek restart starts a fresh stream: clear the reconnect
            // budget - a `continue 'outer` from the reconnect loop lands here.
            reconnect_attempts = 0;

            // Brief debounce to coalesce rapid seeks
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            let target = writer.take_restart_target().unwrap_or(target);

            if writer.is_cancelled() {
                return;
            }

            let range_header = format!("bytes={target}-");
            crate::vprintln2!("[STREAM] Range restart at byte {target}");
            crate::vprintln3!(
                "[STREAM] Range restart #{} at byte {target} | http_reqs={} elapsed={}",
                range_restarts + 1,
                http_requests,
                format_ms(download_start.elapsed().as_secs_f64() * 1000.0),
            );

            // One read for the whole decision: the pool a reconnect uses and the credential
            // it carries have to come from the same owner, and adoption can land between
            // two reads of it.
            let owner = writer.owner();
            let Some(fetch_url) = owner.fetch_url(&task_canonical_id, &url) else {
                // The retained track is no longer ours: this task is stale.
                return;
            };
            let send_fut = owner
                .http_client()
                .get(&fetch_url)
                .header("Range", &range_header)
                .send();
            let range_resp = tokio::select! {
                biased;
                _ = writer.wait_for_restart_or_cancel() => {
                    if writer.is_cancelled() {
                        return;
                    }
                    crate::vprintln3!("[STREAM] Restart aborted (new restart pending) after {}", format_ms(restart_t0.elapsed().as_secs_f64() * 1000.0));
                    continue 'outer;
                }
                result = send_fut => {
                    match result {
                        Ok(r) => r,
                        Err(e) => {
                            crate::vprintln3!(
                                "[STREAM] DOWNLOAD DIED: range request failed at byte {target} | restarts={range_restarts}: {e}"
                            );
                            writer.finish_with_error(
                                DownloadFailure::Network,
                                format!("range request failed: {e}"),
                            );
                            return;
                        }
                    }
                }
            };

            let cdn = crate::player::cdn_cache_status(&range_resp);
            let ttfb = format_ms(restart_t0.elapsed().as_secs_f64() * 1000.0);
            crate::vprintln2!(
                "[STREAM] TTFB: {} | CDN: {} | Range: bytes={}-",
                ttfb,
                cdn,
                target
            );
            crate::player::log_response_headers(&range_resp, "[NET]   ");

            let status = range_resp.status();
            if status == reqwest::StatusCode::PARTIAL_CONTENT {
                writer.reset_for_range(target);
                range_restarts += 1;
                http_requests += 1;
                (range_resp, target)
            } else if status == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
                writer.finish();
                break;
            } else if status.is_success() {
                // Server ignored Range, restart from beginning
                crate::vprintln2!("[STREAM] Server ignored Range header, restarting from byte 0");
                writer.reset_for_range(0);
                (range_resp, 0u64)
            } else {
                crate::vprintln3!(
                    "[STREAM] DOWNLOAD DIED: range status {status} at byte {target} | restarts={range_restarts}"
                );
                writer.finish_with_error(
                    DownloadFailure::Source,
                    format!("range request status: {status}"),
                );
                return;
            }
        };

        // Staging follows the buffer's cacheability: only a stream from byte 0 can be
        // cached; a Range-based one stages nothing. Re-opening here also covers a
        // server that ignored Range and put us back at 0.
        let mut cipher_sink = if stream_offset == 0 {
            open_cipher_sink(&crate::player::canonical_track_id(&url))
        } else {
            None
        };

        let mut stream = stream_resp.bytes_stream();
        let mut offset = stream_offset;
        let mut decrypt_buf = Vec::with_capacity(128 * 1024);
        let mut chunk_count: u32 = 0;
        let mut bytes_since_restart: u64 = 0;
        let stream_start = std::time::Instant::now();
        let mut last_progress_bytes: u64 = 0;

        loop {
            let chunk_opt = tokio::select! {
                biased;
                _ = writer.wait_for_restart_or_cancel() => {
                    if writer.is_cancelled() {
                        return;
                    }
                    crate::vprintln3!(
                        "[STREAM] Interrupted: new restart after {}KB in {} ({} chunks)",
                        bytes_since_restart / 1024,
                        format_ms(stream_start.elapsed().as_secs_f64() * 1000.0),
                        chunk_count
                    );
                    continue 'outer;
                }
                result = stream.next() => result,
            };

            match chunk_opt {
                Some(Ok(chunk)) => {
                    // Select between governor throttle and restart/cancel.
                    // When the governor throttles playback (buffer full),
                    // we must still respond to seek restarts.
                    tokio::select! {
                        biased;
                        _ = writer.wait_for_restart_or_cancel() => {
                            if writer.is_cancelled() {
                                return;
                            }
                            continue 'outer;
                        }
                        // Re-read per chunk: an adopted preload starts charging the
                        // listener's bucket from the chunk after the handover, where the
                        // fixed preload rate cannot keep a hi-res track fed.
                        _ = GOVERNOR.acquire(writer.owner().traffic_class(), chunk.len() as u32) => {}
                    }

                    decrypt_buf.clear();
                    decrypt_buf.extend_from_slice(&chunk);
                    match decryptor
                        .as_ref()
                        .map(|d| d.decrypt_in_place(&mut decrypt_buf, offset))
                        .unwrap_or(Ok(()))
                    {
                        Ok(()) => {
                            offset += chunk.len() as u64;
                            reconnect_attempts = 0; // progress made; reset the reconnect budget
                            let written = writer.write_counted(&decrypt_buf);
                            // `chunk` is still ciphertext: decryption ran on the copy
                            // in `decrypt_buf`. A discarded write means a restart is
                            // imminent: the file must not diverge from the buffer.
                            cipher_sink = match cipher_sink.take() {
                                Some(sink) if written => sink.append(&chunk).await,
                                _ => None,
                            };
                            chunk_count += 1;
                            bytes_since_restart += chunk.len() as u64;

                            // Log first chunk + progress every 512KB
                            if chunk_count == 1 {
                                crate::vprintln3!(
                                    "[STREAM] First chunk: {}B at {}",
                                    chunk.len(),
                                    format_ms(stream_start.elapsed().as_secs_f64() * 1000.0),
                                );
                            }
                            if !written {
                                crate::vprintln3!(
                                    "[STREAM] Write DISCARDED (restart pending) at {}KB",
                                    bytes_since_restart / 1024
                                );
                            }
                            if bytes_since_restart - last_progress_bytes >= 512 * 1024 {
                                crate::vprintln3!(
                                    "[STREAM] Progress: {}KB in {} ({} chunks)",
                                    bytes_since_restart / 1024,
                                    format_ms(stream_start.elapsed().as_secs_f64() * 1000.0),
                                    chunk_count,
                                );
                                last_progress_bytes = bytes_since_restart;
                            }
                        }
                        Err(e) => {
                            writer.finish_with_error(
                                DownloadFailure::Source,
                                format!("decrypt error: {e}"),
                            );
                            return;
                        }
                    }
                }
                Some(Err(e)) => {
                    // Transient transport error (idle connection died on a long
                    // pause, or a blip): reconnect at `offset` and keep appending -
                    // [base..offset] stays valid, and the decode blocks at the frontier
                    // rather than ending the track. Terminal/exhausted retries end it.
                    if writer.is_cancelled() {
                        return;
                    }
                    crate::vprintln!("[STREAM] Stream error at byte {offset}: {e}");
                    stream = 'reconnect: loop {
                        if writer.has_restart_pending() {
                            continue 'outer; // a seek supersedes the reconnect
                        }
                        reconnect_attempts += 1;
                        if reconnect_attempts > MAX_RECONNECTS {
                            crate::vprintln3!(
                                "[STREAM] DOWNLOAD DIED: {MAX_RECONNECTS} reconnects exhausted at byte {offset} | restarts={range_restarts}"
                            );
                            writer.finish_with_error(
                                DownloadFailure::Network,
                                format!("network error after {MAX_RECONNECTS} reconnects: {e}"),
                            );
                            return;
                        }
                        let backoff =
                            std::time::Duration::from_millis(250 * reconnect_attempts as u64);
                        tokio::select! {
                            biased;
                            _ = writer.wait_for_restart_or_cancel() => {
                                if writer.is_cancelled() {
                                    return;
                                }
                                continue 'outer; // seek/cancel during backoff
                            }
                            _ = tokio::time::sleep(backoff) => {}
                        }
                        let range_header = format!("bytes={offset}-");
                        // One read, for the same reason as the restart path above.
                        let owner = writer.owner();
                        let Some(fetch_url) = owner.fetch_url(&task_canonical_id, &url) else {
                            // The retained track is no longer ours. This task is stale.
                            return;
                        };
                        let send_fut = owner
                            .http_client()
                            .get(&fetch_url)
                            .header("Range", &range_header)
                            .send();
                        // The playback client has no request timeout: race the
                        // send against restart/cancel: a stalled server must not pin
                        // this task past a seek/stop/new-track.
                        let reconnect_resp = tokio::select! {
                            biased;
                            _ = writer.wait_for_restart_or_cancel() => {
                                if writer.is_cancelled() {
                                    return;
                                }
                                continue 'outer; // seek supersedes the reconnect
                            }
                            result = send_fut => result,
                        };
                        match reconnect_resp {
                            Ok(r) if r.status() == reqwest::StatusCode::PARTIAL_CONTENT => {
                                http_requests += 1;
                                crate::vprintln!(
                                    "[STREAM] Reconnected at byte {offset} (attempt {reconnect_attempts})"
                                );
                                break 'reconnect r.bytes_stream();
                            }
                            Ok(r) if r.status() == reqwest::StatusCode::RANGE_NOT_SATISFIABLE => {
                                // offset is at/past EOF: all bytes are already
                                // buffered: this download is complete and
                                // cacheable just like a clean EOF.
                                park_ciphertext(&writer, cipher_sink.take(), stream_offset).await;
                                writer.finish();
                                return;
                            }
                            Ok(r) => {
                                crate::vprintln3!(
                                    "[STREAM] DOWNLOAD DIED: reconnect status {} at byte {offset}",
                                    r.status()
                                );
                                writer.finish_with_error(
                                    DownloadFailure::Source,
                                    format!("reconnect status: {}", r.status()),
                                );
                                return;
                            }
                            Err(send_err) => {
                                crate::vprintln!(
                                    "[STREAM] reconnect attempt {reconnect_attempts} failed: {send_err}"
                                );
                                continue 'reconnect;
                            }
                        }
                    };
                    continue; // inner loop: new stream, buffer + offset intact
                }
                None => {
                    break;
                }
            }
        }

        // Check for restart before finishing
        if writer.has_restart_pending() {
            continue 'outer;
        }

        // If we downloaded from byte 0, the entire file is in the buffer.
        if stream_offset == 0 {
            park_ciphertext(&writer, cipher_sink.take(), stream_offset).await;
            writer.finish();
            break;
        }

        // Partial download (Range restart) reached EOF. The buffer covers
        // [base_offset..EOF]. Mark finished: the decode thread sees EOF and
        // emits "completed". If a backward seek needs data before
        // base_offset, buffer.rs clears `finished` and requests a restart.
        writer.finish();
        crate::vprintln2!(
            "[STREAM] Partial EOF (base={}). Waiting for restart or cancel.",
            stream_offset
        );
        loop {
            writer.wait_for_restart_or_cancel().await;
            if writer.is_cancelled() {
                return;
            }
            if writer.has_restart_pending() {
                break; // will continue 'outer
            }
        }
        continue 'outer;
    }

    let total_ms = download_start.elapsed().as_secs_f64() * 1000.0;
    crate::vprintln!(
        "[STREAM] Complete | {} | {} HTTP requests ({} Range restarts)",
        format_ms(total_ms),
        http_requests,
        range_restarts
    );
}

#[cfg(test)]
#[path = "../../tests/unit/audio/preload.rs"]
// `pub(crate)`: the player-thread tests take the same `PRELOAD_STATE` lock this
// module's tests take. Two private locks would not serialise against each other.
pub(crate) mod tests;
