//! Tests for `src/player/crossfade.rs`, attached to it by `#[path]`.

use super::*;

/// The master volume is the only gain the mixer adds on top of the envelopes. At the first sample
/// of a fade the outgoing envelope is exactly one and the incoming exactly zero: unity volume hands
/// the outgoing sample back on the bits, and half volume halves it. A tolerance would hide the
/// one-ulp drift a stray factor next to `volume` produces.
#[test]
fn the_master_volume_is_the_only_gain_beyond_the_envelopes() {
    let incoming = [0.9f32];
    for volume in [1.0f32, 0.5] {
        for sample in [0.3f32, -0.7, 0.25] {
            let mut dst = [0.0f32];
            mix_frames(&mut dst, &[sample], &incoming, 0, 64, volume);
            assert_eq!(
                dst[0].to_bits(),
                (sample * volume).to_bits(),
                "volume {volume}, sample {sample}: got {}",
                dst[0]
            );
        }
    }
}

/// Arming fails closed on an off setting, an unknown length or a zero rate, and fires from the
/// start of the window through the end of the track. A track shorter than the fade would fade in
/// from before it started: play it normally.
#[test]
fn arming_needs_a_known_length_a_nonzero_setting_and_the_window() {
    // 44.1 kHz stereo: one second is 88_200 samples.
    let total = 44_100u64 * 2 * 180; // three minutes
    let six_secs = 44_100u64 * 2 * 6;
    let short = 44_100u64 * 2 * 4;
    for (what, produced, length, rate, secs, expected) in [
        ("off by setting", total - 88_200, total, 44_100, 0, false),
        ("unknown length", 1_000, 0, 44_100, 6, false),
        ("zero rate", total - 88_200, total, 0, 6, false),
        (
            "well before the window",
            total - six_secs - 88_200,
            total,
            44_100,
            6,
            false,
        ),
        (
            "exactly at the boundary",
            total - six_secs,
            total,
            44_100,
            6,
            true,
        ),
        ("inside the window", total - 44_100, total, 44_100, 6, true),
        ("past the end", total + 10, total, 44_100, 6, true),
        (
            "shorter than the fade",
            short - 100,
            short,
            44_100,
            6,
            false,
        ),
    ] {
        assert_eq!(
            crossfade_should_arm(produced, length, rate, 2, secs),
            expected,
            "{what}"
        );
    }
}

/// The fade honours the setting but never outlasts what the outgoing track has left: left at six
/// with two seconds to go, the outgoing ring would empty before the swap and the drain check would
/// tear the stream down mid-fade. Under a second there is nothing worth hearing; it is refused.
#[test]
fn a_fade_is_shortened_to_the_track_left_and_refused_when_absurdly_short() {
    // 44.1 kHz stereo.
    let per_second = 44_100u64 * 2;
    let samples = |secs: u64| Some((per_second * secs) as usize);
    for (what, rate, secs, remaining, expected) in [
        ("plenty left", per_second, 6, per_second * 60, samples(6)),
        ("armed late", per_second, 6, per_second * 2, samples(2)),
        (
            "exactly the setting is not late",
            per_second,
            6,
            per_second * 6,
            samples(6),
        ),
        ("just under a second", per_second, 6, per_second - 1, None),
        ("nothing left", per_second, 6, 0, None),
        (
            "exactly one second is still a fade",
            per_second,
            6,
            per_second,
            samples(1),
        ),
        ("zero rate", 0, 6, per_second * 10, None),
        ("zero setting", per_second, 0, per_second * 10, None),
    ] {
        assert_eq!(fade_len_samples(rate, secs, remaining), expected, "{what}");
    }
}

/// The real-time callback mixes its buffer in fixed-size passes, because cpal declines to
/// bound the length it hands one. That only works if the mixer gives the same samples
/// either way. Every gain is derived from the absolute `pos + i` and the return value
/// carries the position over; a chained sequence of passes has to match one pass over
/// the whole buffer on the BITS, not within a tolerance: a tolerance would hide exactly
/// the drift this pins. Several quanta, including ones that cut mid-frame in stereo. The
/// envelope is a function of the sample index, and nothing may hinge on where the cut lands.
#[test]
fn a_chained_sequence_of_passes_mixes_exactly_like_one() {
    let len = 700usize;
    let outgoing: Vec<f32> = (0..512).map(|i| (i as f32 * 0.017).sin()).collect();
    let incoming: Vec<f32> = (0..512).map(|i| (i as f32 * 0.031).cos()).collect();

    let mut one_pass = vec![0.0f32; 512];
    let end_one = mix_frames(&mut one_pass, &outgoing, &incoming, 11, len, 0.75);

    for quantum in [1usize, 7, 64, 511, 512, 1024] {
        let mut chained = vec![0.0f32; 512];
        let mut pos = 11usize;
        let mut at = 0usize;
        for chunk in chained.chunks_mut(quantum) {
            let n = chunk.len();
            pos = mix_frames(
                chunk,
                &outgoing[at..at + n],
                &incoming[at..at + n],
                pos,
                len,
                0.75,
            );
            at += n;
        }
        assert_eq!(
            pos, end_one,
            "quantum {quantum} ended at a different position"
        );
        for (i, (c, o)) in chained.iter().zip(one_pass.iter()).enumerate() {
            assert_eq!(
                c.to_bits(),
                o.to_bits(),
                "quantum {quantum}, sample {i}: {c} against {o}"
            );
        }
    }
}

/// A pass whose ring ran short hands the mixer a short slice, and the padding has to land
/// where a single pass over a source that simply ran short would have put it.
/// Otherwise a starved ring would sound different depending on the quantum, which is the
/// one way the pass split could become audible.
#[test]
fn a_short_outgoing_slice_pads_like_a_source_that_ran_short() {
    let len = 256usize;
    let outgoing = vec![0.5f32; 96];

    let mut one_pass = vec![0.0f32; 128];
    mix_frames(&mut one_pass, &outgoing, &[], 0, len, 1.0);

    // Two passes of 64. The first is fully fed; the second gets the 32 samples that
    // remain and nothing after them, exactly as `drain_into` would hand it.
    let mut chained = vec![0.0f32; 128];
    let pos = mix_frames(&mut chained[..64], &outgoing[..64], &[], 0, len, 1.0);
    mix_frames(&mut chained[64..], &outgoing[64..96], &[], pos, len, 1.0);

    for (i, (c, o)) in chained.iter().zip(one_pass.iter()).enumerate() {
        assert_eq!(c.to_bits(), o.to_bits(), "sample {i}: {c} against {o}");
    }
}

/// The incoming decoder always conformed to the stream's rate (it is spawned with
/// `output_rate: self.sample_rate`). A differing native rate was never a technical
/// obstacle to the fade itself. It was a policy about what plays AFTER the fade: with a
/// per-track stream, the promoted track would be stuck at the previous track's rate for
/// its whole life. A pinned engine rate removes that, and with it the reason to refuse.
/// The equal-rate case has to hold everywhere, or the feature dies on Linux.
#[test]
fn a_differing_rate_is_refused_only_where_the_engine_rate_moves() {
    for (incoming, stream, pinned, expected) in [
        (96_000, 44_100, true, true),
        (96_000, 44_100, false, false),
        (44_100, 44_100, true, true),
        (44_100, 44_100, false, true),
        (96_000, 96_000, true, true),
        (96_000, 96_000, false, true),
    ] {
        assert_eq!(
            crossfade_accepts_rate(incoming, stream, pinned),
            expected,
            "{incoming} against {stream}, pinned {pinned}"
        );
    }
}

/// A fade that outruns its incoming track has to end on audio that exists. `fade_pos`
/// advances by the buffer length whether or not the incoming ring delivered. Without
/// a refit the envelope reaches its end over silence: the outgoing track is already at
/// zero gain and the incoming one has nothing left to carry.
///
/// Both halves of the contract are checked here. Unity must land on the last sample that
/// exists, and the gain must not step at the splice: a step is a click, which is the
/// failure a naive `len` substitution produces.
#[test]
fn a_refitted_fade_reaches_unity_without_a_gain_step() {
    let (len, pos, available) = (1000usize, 400usize, 100usize);
    let before = xfade_in_env(pos, len);

    let (new_pos, new_len) =
        refit_fade(pos, len, available).expect("a fade short of audio must be refitted");

    assert!(
        (xfade_in_env(new_pos, new_len) - before).abs() < 0.01,
        "the gain jumped at the splice: {before} -> {}",
        xfade_in_env(new_pos, new_len)
    );
    assert_eq!(
        new_pos + available,
        new_len,
        "unity has to land on the last sample the incoming track can supply"
    );
    assert!((xfade_in_env(new_len, new_len) - 1.0).abs() < 1e-6);
}

/// A fade is bounded by what has actually landed, not by the track's size. Six seconds
/// of a 4:20 CD FLAC is a fortieth of the file; demanding all of it is what made every
/// song over two minutes forty-three a hard cut on first listen.
#[test]
fn a_fade_is_bounded_by_the_bytes_actually_staged() {
    // 260s at 44.1kHz stereo, 50.9 MB: the byte rate the clamp derives from those two.
    let (per_second, total, duration) = (88_200u64, 50_900_000u64, 260.0f64);
    let six_seconds = 6 * per_second as usize;

    assert_eq!(
        fade_len_from_staged(six_seconds, total, total, duration, per_second),
        six_seconds,
        "a fully staged track carries the whole configured fade"
    );

    // Only two seconds' worth has landed: the fade shortens to it rather than refusing.
    let two_seconds_of_bytes = (total as f64 / duration * 2.0) as u64;
    let shortened = fade_len_from_staged(
        six_seconds,
        two_seconds_of_bytes,
        total,
        duration,
        per_second,
    );
    assert!(
        shortened < six_seconds && shortened > per_second as usize,
        "expected roughly two seconds of fade, got {shortened} samples"
    );

    // Unknown duration or size must not shorten anything: guessing short here costs a
    // fade that would have worked.
    assert_eq!(
        fade_len_from_staged(six_seconds, 1_000, total, 0.0, per_second),
        six_seconds
    );
    assert_eq!(
        fade_len_from_staged(six_seconds, 1_000, 0, duration, per_second),
        six_seconds
    );
}

/// The refit is for a fade that cannot be honoured, never for one that can, and never for an
/// incoming track with nothing left: forcing the position to `len` there drops the outgoing
/// envelope to zero in one callback, and the mixer already degrades it to a plain fade-out.
#[test]
fn a_fade_that_can_be_honoured_or_has_no_audio_left_is_not_refitted() {
    for (what, pos, len, available) in [
        ("audio to spare", 400, 1000, 600),
        ("plenty of audio", 400, 1000, 5000),
        ("a finished fade", 1000, 1000, 0),
        ("no audio left at all", 400, 1000, 0),
    ] {
        assert_eq!(refit_fade(pos, len, available), None, "{what}");
    }
}
