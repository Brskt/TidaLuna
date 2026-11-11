use std::sync::atomic::{
    AtomicBool, AtomicU32, AtomicU64,
    Ordering::{Relaxed, Release},
};
use std::sync::{Arc, Mutex};

/// The incoming track's ring plus the fade length, handed to the callback when a
/// crossfade arms. `None` outside a crossfade, which is the state that keeps the
/// output path arithmetically identical to a build without this feature.
pub(in crate::player::thread) struct CrossfadeSlot {
    pub consumer: rtrb::Consumer<f32>,
    pub len_samples: usize,
}

/// The three handles the control thread and the audio callback share to run a
/// fade. Grouped because they are only ever meaningful together: an offer, the
/// retraction of an adopted one, and the completion word.
#[derive(Clone)]
pub(in crate::player::thread) struct CrossfadeLink {
    pub attach: Arc<Mutex<Option<CrossfadeSlot>>>,
    pub cancel: Arc<AtomicBool>,
    /// Set by the player thread once the OUTGOING decoder has actually reached its
    /// end. The callback cannot tell an empty ring from a stalled one, and swapping
    /// on emptiness alone would retire a decoder that still had audio to produce.
    pub out_eof: Arc<AtomicBool>,
    /// Set by the player thread once the INCOMING decoder can produce no more. The callback
    /// sees only ring occupancy, which is mute about cause, and "late" and "over" call for
    /// opposite answers: waiting out a late ring costs a moment of the fade, waiting out a
    /// finished one holds the outgoing track open against silence to the end of the envelope.
    pub in_eof: Arc<AtomicBool>,
    /// Published when a fade completes: the generation in the high 16 bits, and in the low 48
    /// how many samples of the INCOMING track were drained. One word, so a fresh generation can
    /// never be read beside a stale origin.
    ///
    /// It counts the incoming drain specifically, because `played_samples` is a lifetime total
    /// that would report the outgoing track's end position for the whole of the new track, and
    /// the nominal fade length over-reports whenever the incoming ring was starved.
    pub done: Arc<AtomicU64>,
}

impl CrossfadeLink {
    pub(super) fn new() -> Self {
        let attach = Arc::new(Mutex::new(None));
        // Locked once here, off the audio thread. On macOS std's `Mutex` is a pthread mutex
        // boxed on its first lock, and the first locker would otherwise be the callback's
        // `try_lock`, allocating on the real-time thread.
        drop(attach.lock());
        Self {
            attach,
            cancel: Arc::new(AtomicBool::new(false)),
            out_eof: Arc::new(AtomicBool::new(false)),
            in_eof: Arc::new(AtomicBool::new(false)),
            done: Arc::new(AtomicU64::new(0)),
        }
    }
}

/// The callback's only way into `attach`, and it never waits: a held lock is `None`, the tick goes
/// on without it. Poison is recovered as it is everywhere else in the player; every state of the
/// guarded `Option` is valid, and reading poison as "never" would leave an adopted fade unable to
/// swap and `pending_crossfade` set for good.
fn try_attach(
    attach: &Mutex<Option<CrossfadeSlot>>,
) -> Option<std::sync::MutexGuard<'_, Option<CrossfadeSlot>>> {
    match attach.try_lock() {
        Ok(guard) => Some(guard),
        Err(std::sync::TryLockError::Poisoned(poisoned)) => Some(poisoned.into_inner()),
        Err(std::sync::TryLockError::WouldBlock) => None,
    }
}

/// Split a value published in `OpenedStream::xfade_done` into `(generation, origin)`.
/// Generation 0 means no fade has completed on this stream yet.
#[inline]
pub(in crate::player::thread) fn unpack_xfade_done(word: u64) -> (u16, u64) {
    (((word >> 48) & 0xFFFF) as u16, word & 0xFFFF_FFFF_FFFF)
}

/// Inverse of [`unpack_xfade_done`]. The origin saturates at 48 bits, which is
/// centuries of samples at any real rate.
#[inline]
pub(in crate::player::thread) fn pack_xfade_done(cur_gen: u16, origin: u64) -> u64 {
    ((cur_gen as u64) << 48) | (origin & 0xFFFF_FFFF_FFFF)
}

/// Scratch the mixer works in per pass, in samples. cpal declines to bound the length it hands
/// a callback (`StreamTrait::buffer_size` is documented as an estimate, and WASAPI recomputes it
/// on every call), so the mixer fixes its own size and slices the callback buffer to match,
/// keeping the real-time path free of a resize. A cost knob rather than a correctness one:
/// passes are chained, and any value mixes the same samples.
pub(super) const MIX_QUANTUM: usize = 8192;

/// Copy up to `dst.len()` samples out of `c`, returning how many were available.
/// The tail is zeroed: a starved ring contributes exact silence to a mix.
fn drain_into(c: &mut rtrb::Consumer<f32>, dst: &mut [f32]) -> usize {
    let to_read = c.slots().min(dst.len());
    if to_read > 0
        && let Ok(chunk) = c.read_chunk(to_read)
    {
        let (s1, s2) = chunk.as_slices();
        let split = s1.len();
        dst[..split].copy_from_slice(s1);
        dst[split..to_read].copy_from_slice(&s2[..to_read - split]);
        chunk.commit_all();
    }
    for s in dst[to_read..].iter_mut() {
        *s = 0.0;
    }
    to_read
}

pub(super) fn build_cpal_callback(
    mut consumer: rtrb::Consumer<f32>,
    volume: Arc<AtomicU32>,
    seek_gen: Arc<AtomicU32>,
    muted: Arc<AtomicBool>,
    mute_ack: Arc<AtomicBool>,
    played_samples: Arc<AtomicU64>,
    xfade: CrossfadeLink,
) -> impl FnMut(&mut [f32], &cpal::OutputCallbackInfo) + Send + 'static {
    let mut local_gen: u32 = 0;
    let mut slot: Option<CrossfadeSlot> = None;
    let mut fade_pos: usize = 0;
    // One rescale per fade. `M` shrinks as the tail is drained; recomputing it every
    // tick would stretch the ending forever instead of reaching it.
    let mut fade_rescaled = false;
    let mut xfade_gen: u16 = 0;
    // What the incoming track has actually been given to the device during the
    // overlap. This, not the stream total, is the new track's position at the swap.
    let mut xfade_in_played: u64 = 0;
    // Sized here, once. `Box<[f32]>` has no `resize`; nothing on the real-time path
    // can allocate however long a buffer a later tick arrives with. `MIX_QUANTUM` carries
    // the reason the size does not track `data.len()`.
    let mut scratch_out: Box<[f32]> = vec![0.0; MIX_QUANTUM].into_boxed_slice();
    let mut scratch_in: Box<[f32]> = vec![0.0; MIX_QUANTUM].into_boxed_slice();
    // `consumer` is borrowed per use, never once at the top: a completed fade
    // reassigns it, and a borrow spanning the body would not compile.
    move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
        if muted.load(Relaxed) {
            let n = consumer.slots();
            if n > 0
                && let Ok(chunk) = consumer.read_chunk(n)
            {
                chunk.commit_all();
            }
            for s in data.iter_mut() {
                *s = 0.0;
            }
            mute_ack.store(true, Relaxed);
            return;
        }

        // Seek gen changed - drain stale samples from before the seek
        let cur_gen = seek_gen.load(Relaxed);
        if cur_gen != local_gen {
            let n = consumer.slots();
            if n > 0
                && let Ok(chunk) = consumer.read_chunk(n)
            {
                chunk.commit_all();
            }
            local_gen = cur_gen;
        }

        // Cancellation is drained BEFORE adoption, and that order is the contract. The
        // flag is a level, not an event: nothing in it names the fade it was raised for,
        // so adopting first let a cancel no tick had drained destroy the NEXT fade. A seek
        // parks the flag for its whole duration while the control thread unmutes and
        // re-arms in the same breath. Draining first can only retire a slot adopted on an
        // EARLIER tick, and the swap stays unconditional so the flag never survives it.
        if xfade.cancel.swap(false, Relaxed) {
            slot = None;
            fade_pos = 0;
            fade_rescaled = false;
            xfade_in_played = 0;
        }
        // Adopt an offered slot. `try_lock` keeps the real-time path free of
        // blocking: a missed tick delays adoption by one buffer, nothing more.
        // Taking it leaves the cell `None`, which from here on means only "nothing
        // new", never "cancel". Cancellation has its own flag above, because one
        // cell serving both made every fade release itself one tick after adoption.
        if let Some(mut attach) = try_attach(&xfade.attach) {
            // Drained again, under the guard. A teardown and a re-arm can both land between
            // the drain above and this lock, and the offer read here is then the NEW fade while
            // the flag still names the old one: adopted unguarded, the next tick's drain would eat
            // it and leave `pending_crossfade` set for good. The teardown raises the flag before it
            // releases this guard, so here the flag is final.
            if xfade.cancel.swap(false, Relaxed) {
                slot = None;
                fade_pos = 0;
                fade_rescaled = false;
                xfade_in_played = 0;
            }
            if attach.is_some() {
                slot = attach.take();
                fade_pos = 0;
                fade_rescaled = false;
                xfade_in_played = 0;
            }
        }

        let v = f32::from_bits(volume.load(Relaxed));

        // Copied out to keep no borrow of `slot` alive across the swap below.
        let Some(mut len_samples) = slot.as_ref().map(|s| s.len_samples) else {
            let to_read = consumer.slots().min(data.len());
            if to_read > 0
                && let Ok(chunk) = consumer.read_chunk(to_read)
            {
                let (s1, s2) = chunk.as_slices();
                let split = s1.len();
                for (dst, src) in data[..split].iter_mut().zip(s1.iter()) {
                    *dst = *src * v;
                }
                for (dst, src) in data[split..to_read].iter_mut().zip(s2.iter()) {
                    *dst = *src * v;
                }
                chunk.commit_all();
                played_samples.fetch_add(to_read as u64, Relaxed);
            }
            for s in data[to_read..].iter_mut() {
                *s = 0.0;
            }
            return;
        };

        // One pass per `MIX_QUANTUM` of the buffer: the scratch never has to match it.
        // `mix_frames` derives every sample's gain from the absolute `fade_pos + i` and
        // returns the position it reached, which is what makes a chained sequence of
        // passes give the same samples as one pass over the whole buffer.
        for chunk in data.chunks_mut(MIX_QUANTUM) {
            let n = chunk.len();
            let out_n = drain_into(&mut consumer, &mut scratch_out[..n]);
            let in_n = match slot.as_mut() {
                Some(active) => drain_into(&mut active.consumer, &mut scratch_in[..n]),
                None => 0,
            };
            xfade_in_played += in_n as u64;

            // A fade that outruns its incoming track finishes on the samples that remain
            // rather than mixing silence. `fade_pos` advances by the pass length whether or
            // not the incoming ring delivered, so without this the envelope runs out over
            // nothing, leaving a hole where the new track should be at full volume. The
            // arithmetic lives in `refit_fade`, pure and tested; only two facts come from
            // here: where the envelope stands, and how much incoming audio will ever arrive.
            if !fade_rescaled
                && xfade.in_eof.load(Relaxed)
                && let Some(active) = slot.as_mut()
            {
                fade_rescaled = true;
                let available = in_n + active.consumer.slots();
                if let Some((pos, len)) =
                    crate::player::crossfade::refit_fade(fade_pos, len_samples, available)
                {
                    active.len_samples = len;
                    len_samples = len;
                    fade_pos = pos;
                }
            }

            fade_pos = crate::player::crossfade::mix_frames(
                chunk,
                &scratch_out[..out_n],
                &scratch_in[..in_n],
                fade_pos,
                len_samples,
                v,
            );
            played_samples.fetch_add(out_n as u64, Relaxed);
        }
        // Three conditions, not one. `fade_pos` alone retires the outgoing ring
        // while it still holds decoded audio nobody heard. Emptiness alone cannot
        // tell "finished" from "stalled mid-download", and would retire a decoder
        // with more of the track to give. Together they can only DELAY the swap,
        // never truncate: a stall extends the fade instead of cutting it short.
        //
        // And under the offer guard, with no cancel raised. The teardown reads `done` under this
        // same guard and raises `cancel` before releasing it, so it either finds this swap already
        // published or has vetoed it; never a swap landing between its read and its kill, which
        // would retire the decoder feeding the ring just promoted. A held guard skips one tick.
        if fade_pos >= len_samples
            && consumer.slots() == 0
            && xfade.out_eof.load(Relaxed)
            && let Some(_offer) = try_attach(&xfade.attach)
            && !xfade.cancel.load(Relaxed)
        {
            // The incoming ring becomes the primary one HERE, inside the callback
            // that owns both. The control thread cannot do it: this closure owns
            // `consumer` by move. Only the track identity is left for the player.
            if let Some(done) = slot.take() {
                consumer = done.consumer;
            }
            // Rebase the clock in the tick that swaps, the only place that knows where the
            // swap fell: the control thread reads `done` up to a poll later, when the counter
            // holds the outgoing total plus whatever the promoted ring has since delivered,
            // two quantities no reader can tell apart. The `Release` below publishes this
            // store, and `crossfade.rs` reads `done` with `Acquire`, so a reader that sees this
            // generation sees this clock.
            played_samples.store(xfade_in_played, Relaxed);
            fade_pos = 0;
            fade_rescaled = false;
            xfade_gen = xfade_gen.wrapping_add(1).max(1);
            // Release: the player thread must not observe this generation beside a
            // stale origin.
            xfade
                .done
                .store(pack_xfade_done(xfade_gen, xfade_in_played), Release);
            xfade_in_played = 0;
        }
    }
}

#[cfg(test)]
#[path = "../../../../tests/unit/player/thread/output/mixer.rs"]
mod tests;
