//! Tests for the resampling and callback halves of `src/player/thread/output.rs`,
//! attached to it by `#[path]`.
//! `AudioPipeline` is `pub(super)` and `build_cpal_callback` is module-private; a child module
//! of `player::thread` reaches both without widening anything. Nothing here opens a device:
//! the pipeline is a pure function of its input, and the callback is a closure over rings.
//!
//! Every case that checks a shape also checks energy. A structural assertion cannot tell a
//! correct result from an empty one, and silence is what a broken resampler produces.

use super::{
    AudioPipeline, CHUNK_SIZE, CrossfadeLink, CrossfadeSlot, MIX_QUANTUM, attempt_order,
    build_cpal_callback, pack_xfade_done, unpack_xfade_done,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::Relaxed};

/// One callback's worth of samples. Any size works: the envelopes are a function of
/// the fade position, not of the buffer length.
const CB_FRAMES: usize = 256;

/// The outgoing track's count before a swap: far above any count a few callbacks could reach.
const OUTGOING_PLAYED: u64 = 1_000_000;

/// The callback ignores its timestamp argument entirely, but the signature demands
/// one. Nothing here opens a device.
fn callback_info() -> cpal::OutputCallbackInfo {
    cpal::OutputCallbackInfo::new(cpal::OutputStreamTimestamp {
        callback: cpal::StreamInstant::ZERO,
        playback: cpal::StreamInstant::ZERO,
    })
}

/// A callback wired to `xfade`, fed an outgoing ring holding `outgoing` repeated.
fn callback_over(
    xfade: &CrossfadeLink,
    outgoing: f32,
    ring_frames: usize,
) -> impl FnMut(&mut [f32], &cpal::OutputCallbackInfo) {
    callback_counting(xfade, outgoing, ring_frames).0
}

/// The same, handing back the `played_samples` counter the callback writes into. The
/// clock a promotion inherits is set inside the callback; this is the only place it
/// can be observed.
fn callback_counting(
    xfade: &CrossfadeLink,
    outgoing: f32,
    ring_frames: usize,
) -> (
    impl FnMut(&mut [f32], &cpal::OutputCallbackInfo),
    Arc<AtomicU64>,
) {
    let played_samples = Arc::new(AtomicU64::new(0));
    let (mut producer, consumer) = rtrb::RingBuffer::<f32>::new(ring_frames);
    for _ in 0..ring_frames {
        producer
            .push(outgoing)
            .expect("the ring was sized for this");
    }
    // Leaked on purpose: the producer must outlive the closure or the consumer reads
    // a disconnected ring, and this is a test, not a pipeline.
    std::mem::forget(producer);
    let callback = build_cpal_callback(
        consumer,
        Arc::new(AtomicU32::new(1.0f32.to_bits())),
        Arc::new(AtomicU32::new(0)),
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
        Arc::clone(&played_samples),
        xfade.clone(),
    );
    (callback, played_samples)
}

/// An incoming ring holding `sample` repeated, offered as a fade of `len_samples`.
fn offer_fade(xfade: &CrossfadeLink, sample: f32, len_samples: usize) {
    offer_fade_sized(xfade, sample, len_samples, CB_FRAMES * 4);
}

/// The same with the ring sized by the caller. A fade driven across several passes needs
/// more than one callback's worth staged to reach its end.
fn offer_fade_sized(xfade: &CrossfadeLink, sample: f32, len_samples: usize, ring_frames: usize) {
    let (mut producer, consumer) = rtrb::RingBuffer::<f32>::new(ring_frames);
    for _ in 0..ring_frames {
        producer.push(sample).expect("the ring was sized for this");
    }
    std::mem::forget(producer);
    *xfade.attach.lock().unwrap() = Some(CrossfadeSlot {
        consumer,
        len_samples,
    });
}

#[test]
fn a_stale_cancel_cannot_eat_a_fade_adopted_after_it() {
    // The flag is a level, and nothing in it names the fade it was raised for. The
    // mute branch returns before both checks; a seek parks one for its whole
    // duration while the control thread unmutes and re-arms in the same breath.
    // Adopting before draining it killed that fresh fade on arrival, silently,
    // since only the audio thread ever knew the slot was gone.
    let xfade = CrossfadeLink::new();
    xfade.cancel.store(true, Relaxed);
    // Silence outgoing, a tone incoming: with the fade alive the rising envelope
    // lets the incoming track through, and a fade that was eaten leaves pure silence.
    offer_fade(&xfade, 1.0, CB_FRAMES);
    let mut callback = callback_over(&xfade, 0.0, CB_FRAMES * 4);

    let mut data = vec![0.0f32; CB_FRAMES];
    callback(&mut data, &callback_info());

    assert!(
        peak(&data) > 0.0,
        "the fade offered after the cancel was destroyed on adoption"
    );
    assert!(
        !xfade.cancel.load(Relaxed),
        "the flag must never survive the tick that saw it"
    );
}

#[test]
fn a_cancel_still_retires_the_fade_it_was_raised_for() {
    // The other half of the contract: draining first must not cost cancellation its
    // job. A skip, a seek or a stop mid-fade still has to drop the slot on the next
    // tick, or the callback keeps blending a track the player has already retired.
    let xfade = CrossfadeLink::new();
    // A tone outgoing, silence incoming: while the fade runs the descending envelope
    // attenuates it, and once the fade is dropped it comes through untouched.
    offer_fade(&xfade, 0.0, CB_FRAMES * 2);
    let mut callback = callback_over(&xfade, 1.0, CB_FRAMES * 4);

    let mut data = vec![0.0f32; CB_FRAMES];
    callback(&mut data, &callback_info());
    assert!(
        data.iter().any(|s| *s < 1.0),
        "the fade never took: nothing was attenuated"
    );

    xfade.cancel.store(true, Relaxed);
    callback(&mut data, &callback_info());

    assert!(
        data.iter().all(|s| (*s - 1.0).abs() < f32::EPSILON),
        "the cancelled fade kept shaping the audio"
    );
}

#[test]
fn a_fade_longer_than_one_pass_keeps_its_envelope_across_the_passes() {
    // The scratch is fixed at `MIX_QUANTUM`: a buffer bigger than that is mixed in
    // several passes. The fade position has to carry over between them: reset per pass,
    // the envelope would restart and the outgoing track would come back at full gain
    // half way down its own fade-out. The fade length lands on the seam on purpose.
    let xfade = CrossfadeLink::new();
    let total = MIX_QUANTUM * 2;
    // A tone outgoing against silence incoming leaves the descending envelope readable
    // straight off the output one sample at a time.
    offer_fade_sized(&xfade, 0.0, MIX_QUANTUM, total);
    let mut callback = callback_over(&xfade, 1.0, total * 2);

    let mut data = vec![0.0f32; total];
    callback(&mut data, &callback_info());

    assert!(
        data[0] > 0.99,
        "the fade should open at full outgoing gain, got {}",
        data[0]
    );
    assert!(
        data[MIX_QUANTUM - 1].abs() < 1e-3,
        "the envelope had not reached zero by the end of the first pass, got {}",
        data[MIX_QUANTUM - 1]
    );
    // One sample is enough to catch a per-pass reset: it would put full gain right back
    // at the head of the second pass.
    assert!(
        data[MIX_QUANTUM].abs() < 1e-3,
        "the second pass restarted the envelope, got {}",
        data[MIX_QUANTUM]
    );
    assert!(
        peak(&data[MIX_QUANTUM..]) < 1e-3,
        "audio leaked past the end of the fade"
    );
}

#[test]
fn a_buffer_that_changes_length_between_ticks_stays_one_continuous_fade() {
    // cpal refuses to bound the length it hands a callback, and WASAPI recomputes it on
    // every call from the endpoint padding; a later tick can ask for more than the
    // first one did. The scratch no longer tracks that length, which is what removes the
    // resize; what has to survive is the fade running continuously across ticks whose
    // sizes straddle the quantum in both directions.
    let xfade = CrossfadeLink::new();
    let len_samples = MIX_QUANTUM * 3;
    offer_fade_sized(&xfade, 0.0, len_samples, MIX_QUANTUM * 8);
    let mut callback = callback_over(&xfade, 1.0, MIX_QUANTUM * 16);

    let mut consumed = 0usize;
    let mut previous = f32::INFINITY;
    for frames in [64usize, MIX_QUANTUM + 1, 32, MIX_QUANTUM * 2, 7] {
        let mut data = vec![0.0f32; frames];
        callback(&mut data, &callback_info());
        // The outgoing envelope only ever descends: the first sample of a tick can
        // never sit above the last sample of the tick before it.
        assert!(
            data[0] <= previous,
            "the envelope jumped back up at a tick of {frames}: {} after {previous}",
            data[0]
        );
        previous = data[frames - 1];
        consumed += frames;
    }
    assert!(
        consumed > len_samples,
        "the ticks never drove the fade to its end"
    );
    assert!(
        previous.abs() < 1e-3,
        "the fade never finished, last sample {previous}"
    );
}

#[test]
fn a_fade_does_not_swap_while_the_offer_lock_is_held() {
    let xfade = CrossfadeLink::new();
    // Two callbacks of fade against two of outgoing audio: the first tick adopts and mixes
    // half, the second is the one that reaches all three swap conditions.
    offer_fade_sized(&xfade, 1.0, CB_FRAMES * 2, CB_FRAMES * 4);
    let (mut callback, played_samples) = callback_counting(&xfade, 1.0, CB_FRAMES * 2);
    xfade.out_eof.store(true, Relaxed);
    played_samples.store(OUTGOING_PLAYED, Relaxed);
    let mut data = vec![0.0f32; CB_FRAMES];
    callback(&mut data, &callback_info());

    let held = xfade.attach.lock().unwrap();
    callback(&mut data, &callback_info());
    assert_eq!(
        unpack_xfade_done(xfade.done.load(Relaxed)).0,
        0,
        "the fade swapped while the teardown held the guard"
    );
    assert!(
        played_samples.load(Relaxed) >= OUTGOING_PLAYED,
        "the clock was rebased by a swap that should not have happened"
    );

    drop(held);
    callback(&mut data, &callback_info());
    let (cur_gen, in_played) = unpack_xfade_done(xfade.done.load(Relaxed));
    assert!(
        cur_gen > 0,
        "the fade never swapped once the guard was free"
    );
    assert_eq!(played_samples.load(Relaxed), in_played);
}

#[test]
fn a_poisoned_offer_lock_still_adopts_and_swaps() {
    let xfade = CrossfadeLink::new();
    offer_fade_sized(&xfade, 1.0, CB_FRAMES, CB_FRAMES * 4);
    let (mut callback, _played) = callback_counting(&xfade, 1.0, CB_FRAMES);
    xfade.out_eof.store(true, Relaxed);
    let attach = Arc::clone(&xfade.attach);
    let _ = std::thread::spawn(move || {
        let _guard = attach.lock().unwrap();
        panic!("poisoning the offer lock on purpose");
    })
    .join();
    assert!(xfade.attach.is_poisoned());

    let mut data = vec![0.0f32; CB_FRAMES];
    callback(&mut data, &callback_info());

    assert!(
        unpack_xfade_done(xfade.done.load(Relaxed)).0 > 0,
        "a poisoned guard kept the fade from adopting or swapping"
    );
}

#[test]
fn the_swap_rebases_the_clock_to_the_incoming_track_before_it_publishes() {
    // The control thread reads `done` up to a poll later, and by then the counter
    // holds the outgoing total plus whatever the promoted ring has delivered since,
    // two quantities it cannot separate. The tick that swaps has to leave the
    // clock already naming the incoming track, or that interval is lost when the
    // promotion finally lands.
    let xfade = CrossfadeLink::new();
    // A fade one callback long against an outgoing ring holding exactly that: the
    // tick which mixes it also empties it, and emptiness is the second of the three
    // conditions the swap waits on.
    offer_fade_sized(&xfade, 1.0, CB_FRAMES, CB_FRAMES * 4);
    let (mut callback, played_samples) = callback_counting(&xfade, 1.0, CB_FRAMES);
    xfade.out_eof.store(true, Relaxed);
    // Where the outgoing track had got to. None of it belongs to the incoming one,
    // and starting from zero would hide the defect behind a coincidence: a single
    // pass credits an outgoing drain that happens to equal the incoming one.
    played_samples.store(OUTGOING_PLAYED, Relaxed);

    let mut data = vec![0.0f32; CB_FRAMES];
    callback(&mut data, &callback_info());

    let (cur_gen, in_played) = unpack_xfade_done(xfade.done.load(Relaxed));
    assert!(
        cur_gen > 0,
        "the fade never completed, so nothing was rebased"
    );
    assert_eq!(
        played_samples.load(Relaxed),
        in_played,
        "the swapping tick left the outgoing track's total on the clock"
    );

    // The tick after it counts on from that base rather than restarting or resuming
    // the outgoing total.
    callback(&mut data, &callback_info());
    assert_eq!(
        played_samples.load(Relaxed),
        in_played + CB_FRAMES as u64,
        "the promoted ring's samples did not accumulate onto the rebased clock"
    );
}

#[test]
fn a_fade_completion_survives_the_round_trip() {
    // Generation and origin share one word: the player thread can never read a
    // fresh generation beside a stale origin. That only holds if neither field
    // bleeds into the other.
    for (cur_gen, origin) in [(1u16, 0u64), (1, 529_200), (7, 21_168_000), (u16::MAX, 1)] {
        assert_eq!(
            unpack_xfade_done(pack_xfade_done(cur_gen, origin)),
            (cur_gen, origin)
        );
    }
}

#[test]
fn the_origin_never_bleeds_into_the_generation() {
    // 48 bits is centuries of samples at any real rate, but a value past that must
    // truncate rather than corrupt the generation beside it.
    let (cur_gen, origin) = unpack_xfade_done(pack_xfade_done(3, u64::MAX));
    assert_eq!(cur_gen, 3);
    assert_eq!(origin, 0xFFFF_FFFF_FFFF);
}

#[test]
fn the_heap_probe_sees_an_allocation_and_its_free() {
    // The instrument's own control: a probe that counted nothing would pass every
    // zero-allocation test below without looking.
    let calls = crate::player::heap_probe::heap_calls_during(|| {
        drop(std::hint::black_box(Box::new(7u64)));
    });
    assert_eq!(calls, 2, "one allocation and one free, counted {calls}");
}

#[test]
fn the_first_tick_of_a_fresh_stream_touches_no_heap() {
    // A stream's first tick is the first thing that ever runs on its audio thread; anything
    // created lazily and touched first here is built on the real-time path. No
    // fade is offered: an offer would lock the link off-thread first and hide exactly that.
    let xfade = CrossfadeLink::new();
    let mut callback = callback_over(&xfade, 1.0, CB_FRAMES * 4);
    let mut data = vec![0.0f32; CB_FRAMES];

    let calls = crate::player::heap_probe::heap_calls_during(|| {
        callback(&mut data, &callback_info());
    });

    assert!(peak(&data) > 0.0, "the tick never played the outgoing ring");
    assert_eq!(
        calls, 0,
        "the first tick called the allocator {calls} times"
    );
}

#[test]
fn a_fade_from_adoption_to_swap_touches_no_heap() {
    // The whole life of a fade inside the callback: adoption, the mix, the swap that
    // retires the outgoing consumer. Its producer stays alive, like the parked outgoing decoder's
    // in the pipeline: retiring it frees nothing either.
    let xfade = CrossfadeLink::new();
    offer_fade_sized(&xfade, 1.0, CB_FRAMES * 2, CB_FRAMES * 4);
    let mut callback = callback_over(&xfade, 1.0, CB_FRAMES * 2);
    xfade.out_eof.store(true, Relaxed);
    let mut data = vec![0.0f32; CB_FRAMES];

    let calls = crate::player::heap_probe::heap_calls_during(|| {
        for _ in 0..3 {
            callback(&mut data, &callback_info());
        }
    });

    assert!(
        unpack_xfade_done(xfade.done.load(Relaxed)).0 > 0,
        "the fade never swapped inside the probed ticks"
    );
    assert_eq!(calls, 0, "the fade called the allocator {calls} times");
}

#[test]
fn a_cancelled_fade_touches_no_heap_while_its_producer_lives() {
    // The cancel drain drops the adopted slot on the audio thread. With its producer
    // alive that drop only flags the ring; the free belongs to whichever side lets go last.
    let xfade = CrossfadeLink::new();
    offer_fade(&xfade, 0.0, CB_FRAMES * 2);
    let mut callback = callback_over(&xfade, 1.0, CB_FRAMES * 4);
    let mut data = vec![0.0f32; CB_FRAMES];

    let calls = crate::player::heap_probe::heap_calls_during(|| {
        callback(&mut data, &callback_info());
        xfade.cancel.store(true, Relaxed);
        callback(&mut data, &callback_info());
    });

    assert!(
        data.iter().all(|s| (*s - 1.0).abs() < f32::EPSILON),
        "the cancel never retired the fade"
    );
    assert_eq!(calls, 0, "the cancel called the allocator {calls} times");
}

const SRC_RATE: u32 = 44_100;
const OUT_RATE: u32 = 48_000;
const TONE_HZ: f32 = 1_000.0;
/// The sinc window has to fill before the output settles; measuring across that ramp would
/// count crossings the signal does not have.
const SETTLE_FRAMES: usize = 2_048;

fn pipeline(src_ch: usize, out_ch: usize) -> AudioPipeline {
    AudioPipeline::new(SRC_RATE, OUT_RATE, src_ch, out_ch).expect("pipeline")
}

/// Interleaved sine, the same signal on every channel.
fn sine(freq: f32, rate: u32, frames: usize, channels: usize) -> Vec<f32> {
    let mut out = Vec::with_capacity(frames * channels);
    for f in 0..frames {
        let s = (std::f32::consts::TAU * freq * f as f32 / rate as f32).sin();
        for _ in 0..channels {
            out.push(s);
        }
    }
    out
}

fn peak(samples: &[f32]) -> f32 {
    samples.iter().fold(0.0f32, |m, s| m.max(s.abs()))
}

/// Frequency of one channel, from its rising zero crossings. A sine crosses zero upward once
/// per period; the count over a known duration is the frequency. Preferred to an FFT here
/// because it needs no window choice and no dependency. It says nothing about amplitude: a
/// tone attenuated to nothing still crosses zero on schedule, hence the `peak` every caller
/// pairs it with.
fn dominant_hz(interleaved: &[f32], channels: usize, channel: usize, rate: u32) -> f32 {
    let frames = interleaved.len() / channels;
    assert!(frames > 1, "not enough frames to estimate a frequency");
    let mut rising = 0usize;
    let mut prev = interleaved[channel];
    for f in 1..frames {
        let cur = interleaved[f * channels + channel];
        if prev < 0.0 && cur >= 0.0 {
            rising += 1;
        }
        prev = cur;
    }
    rising as f32 * rate as f32 / frames as f32
}

#[test]
fn resampling_stretches_the_frame_count_by_the_rate_ratio() {
    let mut pipe = pipeline(2, 2);
    let frames_in = SRC_RATE as usize;
    let out = pipe
        .process(&sine(TONE_HZ, SRC_RATE, frames_in, 2))
        .unwrap();

    let ratio = OUT_RATE as f64 / SRC_RATE as f64;
    let frames_out = out.len() as f64 / 2.0;
    let expected = frames_in as f64 * ratio;

    // One-sided: `process` emits whole chunks only, and can fall short of the ratio but
    // never exceed it. The shortfall is the sub-chunk tail left in the accumulator plus the
    // filter's lead-in. Ratio precision is `a_resampled_sine_keeps_its_frequency`'s job.
    let max_shortfall = CHUNK_SIZE as f64 * ratio + 256.0;
    assert!(
        frames_out <= expected,
        "{frames_out} frames out of {expected} expected: the pipeline cannot invent audio"
    );
    assert!(
        expected - frames_out <= max_shortfall,
        "short by {} frames, more than the {max_shortfall:.0} a chunk tail explains",
        expected - frames_out
    );
}

#[test]
fn a_resampled_sine_keeps_its_frequency_and_its_level() {
    let mut pipe = pipeline(2, 2);
    let out = pipe
        .process(&sine(TONE_HZ, SRC_RATE, SRC_RATE as usize, 2))
        .unwrap();

    let settled = &out[SETTLE_FRAMES * 2..];
    let hz = dominant_hz(settled, 2, 0, OUT_RATE);
    assert!(
        (hz - TONE_HZ).abs() < TONE_HZ * 0.02,
        "got {hz} Hz where the source carried {TONE_HZ}"
    );
    assert!(
        (peak(settled) - 1.0).abs() < 0.1,
        "peak {} for a unit sine",
        peak(settled)
    );
}

#[test]
fn flush_emits_the_partial_chunk_as_audio_not_padding() {
    let mut pipe = pipeline(2, 2);
    // Warm the filter on SILENCE, then accumulate a tone. The two signals have to differ:
    // warming with the same tone leaves the filter ringing loudly enough that a flush
    // reading none of the accumulated frames still comes out at full level.
    pipe.process(&vec![0.0f32; CHUNK_SIZE * 3 * 2]).unwrap();
    assert!(
        pipe.process(&sine(TONE_HZ, SRC_RATE, CHUNK_SIZE / 2, 2))
            .unwrap()
            .is_empty(),
        "a sub-chunk tail should stay in the accumulator"
    );

    let flushed = pipe.flush().unwrap();
    assert!(!flushed.is_empty(), "the partial chunk never came out");
    assert_eq!(flushed.len() % 2, 0, "a frame was cut in half");
    // The assertion that matters: `flush` always returns a full output buffer, which lets
    // a zero-filled one satisfy every check above while the tail of each track is lost.
    assert!(
        peak(&flushed) > 0.5,
        "peak {}: the tail came out as padding, not audio",
        peak(&flushed)
    );
    assert!(pipe.flush().unwrap().is_empty(), "the accumulator refilled");
}

#[test]
fn reset_drops_the_filter_history_a_seek_would_bleed() {
    let mut pipe = pipeline(2, 2);
    // Load the sinc history with a full-scale tone, leaving a partial chunk accumulated.
    pipe.process(&sine(TONE_HZ, SRC_RATE, CHUNK_SIZE * 3 + 100, 2))
        .unwrap();

    pipe.reset();

    // Silence in: whatever comes out is history the reset failed to drop. Without
    // `resampler.reset()` the pre-seek tone rings on through the first output frames.
    let out = pipe.process(&vec![0.0f32; CHUNK_SIZE * 3 * 2]).unwrap();
    assert!(
        peak(&out) < 0.01,
        "peak {}: audio from before the reset bled through",
        peak(&out)
    );
    assert!(pipe.flush().unwrap().is_empty(), "the accumulator survived");
}

#[test]
fn a_narrower_output_takes_the_first_channels_and_drops_the_rest() {
    let mut pipe = pipeline(2, 1);
    // The tone on the left, silence on the right: truncation keeps the tone at full
    // amplitude, where a downmix would halve it.
    let mut input = Vec::with_capacity(SRC_RATE as usize * 2);
    for f in 0..SRC_RATE as usize {
        input.push((std::f32::consts::TAU * TONE_HZ * f as f32 / SRC_RATE as f32).sin());
        input.push(0.0);
    }
    let out = pipe.process(&input).unwrap();
    let settled = &out[SETTLE_FRAMES..];

    let hz = dominant_hz(settled, 1, 0, OUT_RATE);
    assert!((hz - TONE_HZ).abs() < TONE_HZ * 0.02, "got {hz} Hz");
    assert!(
        peak(settled) > 0.9,
        "peak {}: the channels were mixed, not taken",
        peak(settled)
    );
}

#[test]
fn a_wider_output_duplicates_a_mono_source() {
    let mut pipe = pipeline(1, 2);
    let out = pipe
        .process(&sine(TONE_HZ, SRC_RATE, SRC_RATE as usize, 1))
        .unwrap();

    assert_eq!(out.len() % 2, 0, "a frame was cut in half");
    let settled = &out[SETTLE_FRAMES * 2..];
    // Equality alone holds for two silent channels; the tone has to be there too.
    assert!(
        peak(settled) > 0.9,
        "peak {}: silence duplicated is still silence",
        peak(settled)
    );
    for (frame, pair) in settled.as_chunks::<2>().0.iter().enumerate() {
        assert_eq!(pair[0], pair[1], "the channels diverged at frame {frame}");
    }
}

#[test]
fn channels_beyond_a_multichannel_source_are_filled_with_silence() {
    // Rate parity leaves no resampler between the input and the assertion: this is the
    // remap alone, on the 2 -> 6 layout a multichannel endpoint asks for.
    let mut pipe = AudioPipeline::new(SRC_RATE, SRC_RATE, 2, 6).expect("pipeline");
    assert!(!pipe.resamples(), "matching rates need no filter");

    let out = pipe.process(&[0.25, -0.5, 0.75, -1.0]).unwrap();

    assert_eq!(out.len(), 12, "two frames of six channels");
    assert_eq!(&out[..2], &[0.25, -0.5], "the source pair moved through");
    assert_eq!(&out[2..6], &[0.0; 4], "the extra channels carry silence");
    assert_eq!(&out[6..8], &[0.75, -1.0]);
    assert_eq!(&out[8..], &[0.0; 4]);
}

#[test]
fn matching_rates_skip_the_resampler_and_pass_samples_through() {
    let mut pipe = AudioPipeline::new(SRC_RATE, SRC_RATE, 2, 2).expect("pipeline");
    assert!(!pipe.resamples());

    let input = sine(TONE_HZ, SRC_RATE, 512, 2);
    // Bit-identical: a sinc at ratio 1.0 would low-pass and delay these samples instead.
    assert_eq!(pipe.process(&input).unwrap(), input);
    assert!(pipe.flush().unwrap().is_empty(), "nothing was held back");
}

/// The order the stream is opened in decides who resamples. On Windows shared mode and
/// on macOS the device's default IS the rate the audio server runs at; targeting it
/// leaves the server nothing to convert. On Linux cpal cannot learn PipeWire's real
/// graph rate (it reports 48000 from two unrelated heuristics), and aiming at it would
/// stack our conversion on top of PipeWire's. `attempt_order` takes that as a plain
/// `pinned` argument instead of reading `ENGINE_RATE_IS_PINNED` itself, leaving both
/// orders asserted here on every host. Either order keeps the other candidate as the
/// fallback, for a device that refuses its own advertised default.
#[test]
fn the_device_default_is_tried_first_only_where_it_means_something() {
    let source = (44_100u32, 2u16);
    let device = (192_000u32, 2u16);

    assert_eq!(
        attempt_order(source, device, true),
        [device, source],
        "the device's own rate has to win"
    );
    assert_eq!(
        attempt_order(source, device, false),
        [source, device],
        "Linux keeps the source-first policy"
    );
}
