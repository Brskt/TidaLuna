//! Tests for the resampling half of `src/player/thread/output.rs`, attached to it by `#[path]`.
//! `AudioPipeline` is `pub(super)`, and a child module of `player::thread` reaches it without
//! widening anything. Nothing here opens a device: the pipeline is a pure function of its input.
//!
//! Every case that checks a shape also checks energy. A structural assertion cannot tell a
//! correct result from an empty one, and silence is what a broken resampler produces.

use super::{AudioPipeline, CHUNK_SIZE};

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
/// per period, so the count over a known duration is the frequency. Preferred to an FFT here
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
