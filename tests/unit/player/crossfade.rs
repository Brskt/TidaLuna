//! Tests for `src/player/crossfade.rs`, attached to it by `#[path]`.

use super::*;

#[test]
fn envelopes_run_the_full_range() {
    assert!((xfade_out_env(0, 441) - 1.0).abs() < 1e-6);
    assert!(xfade_out_env(441, 441).abs() < 1e-6);
    assert!(xfade_in_env(0, 441).abs() < 1e-6);
    assert!((xfade_in_env(441, 441) - 1.0).abs() < 1e-6);
}

#[test]
fn envelopes_are_equal_power_not_equal_gain() {
    // The invariant that separates this pair from declick.rs: the SQUARES sum to
    // one; summed power stays flat for two uncorrelated sources.
    for p in [0, 55, 110, 220, 330, 441] {
        let o = xfade_out_env(p, 441);
        let i = xfade_in_env(p, 441);
        assert!(
            (o * o + i * i - 1.0).abs() < 1e-5,
            "power not flat at {p}: {o}^2 + {i}^2"
        );
    }
    // At the midpoint they are NOT the equal-gain 0.5, which is the bug avoided.
    let mid = xfade_out_env(220, 441);
    assert!((mid - 0.5).abs() > 0.15, "midpoint {mid} looks equal-gain");
}

#[test]
fn zero_len_does_not_divide_by_zero() {
    assert!(xfade_in_env(0, 0).is_finite());
    assert!(xfade_out_env(0, 0).is_finite());
}

#[test]
fn mix_sums_both_sources_and_advances_the_position() {
    let outgoing = vec![1.0f32; 8];
    let incoming = vec![1.0f32; 8];
    let mut dst = vec![0.0f32; 8];
    let end = mix_frames(&mut dst, &outgoing, &incoming, 0, 64, 1.0);
    assert_eq!(end, 8);
    // Every sample is a real blend of the two, never one or the other alone.
    for s in &dst {
        assert!(*s > 0.9 && *s <= 1.45, "unexpected mixed sample {s}");
    }
}

#[test]
fn a_missing_incoming_source_leaves_the_outgoing_untouched_in_shape() {
    // A starved second ring contributes exact zeros. `x + 0.0 == x` in IEEE-754,
    // the outgoing side is only ever scaled, never perturbed.
    let outgoing = vec![0.25f32; 4];
    let incoming: Vec<f32> = Vec::new();
    let mut dst = vec![0.0f32; 4];
    mix_frames(&mut dst, &outgoing, &incoming, 0, 64, 1.0);
    for (i, s) in dst.iter().enumerate() {
        let expected = 0.25 * xfade_out_env(i, 64);
        assert!((s - expected).abs() < 1e-6, "sample {i}: {s} vs {expected}");
    }
}

#[test]
fn volume_one_is_bit_exact() {
    // The no-crossfade guarantee on cpal is FP identity at unity gain.
    let outgoing = vec![0.123_456_79f32; 4];
    let incoming: Vec<f32> = Vec::new();
    let mut dst = vec![0.0f32; 4];
    // Position 0 with an absent incoming source scales by exactly 1.0.
    mix_frames(&mut dst, &outgoing, &incoming, 0, 64, 1.0);
    assert_eq!(dst[0], 0.123_456_79f32);
}

#[test]
fn mix_clamps_the_position_at_the_fade_length() {
    let outgoing = vec![1.0f32; 8];
    let incoming = vec![1.0f32; 8];
    let mut dst = vec![0.0f32; 8];
    let end = mix_frames(&mut dst, &outgoing, &incoming, 60, 64, 1.0);
    assert_eq!(end, 64, "position must saturate at len, not run past it");
}

#[test]
fn arming_needs_a_known_length_and_a_nonzero_setting() {
    // 44.1 kHz stereo: one second is 88_200 samples.
    let total = 44_100u64 * 2 * 180; // three minutes
    // Off by setting.
    assert!(!crossfade_should_arm(total - 88_200, total, 44_100, 2, 0));
    // Unknown length fails closed.
    assert!(!crossfade_should_arm(1_000, 0, 44_100, 2, 6));
    // Zero rate fails closed rather than dividing.
    assert!(!crossfade_should_arm(total - 88_200, total, 0, 2, 6));
}

#[test]
fn arming_fires_only_inside_the_window() {
    let total = 44_100u64 * 2 * 180;
    let six_secs = 44_100u64 * 2 * 6;
    // Well before the window.
    assert!(!crossfade_should_arm(
        total - six_secs - 88_200,
        total,
        44_100,
        2,
        6
    ));
    // Exactly at the boundary.
    assert!(crossfade_should_arm(total - six_secs, total, 44_100, 2, 6));
    // Inside it.
    assert!(crossfade_should_arm(total - 44_100, total, 44_100, 2, 6));
    // Past the end never panics and stays true.
    assert!(crossfade_should_arm(total + 10, total, 44_100, 2, 6));
}

#[test]
fn a_track_shorter_than_the_fade_never_arms() {
    // A four-second track with a six-second fade would fade in from before it
    // started. Fail closed: play it normally.
    let total = 44_100u64 * 2 * 4;
    assert!(!crossfade_should_arm(total - 100, total, 44_100, 2, 6));
}

#[test]
fn a_fade_never_outlasts_what_the_outgoing_track_has_left() {
    // 44.1 kHz stereo.
    let per_second = 44_100u64 * 2;
    // Plenty of track left: the setting is honoured as-is.
    assert_eq!(
        fade_len_samples(per_second, 6, per_second * 60),
        Some((per_second * 6) as usize)
    );
    // Armed late, only two seconds left: the fade shrinks to two. Left at six, the
    // outgoing ring would empty four seconds before the swap could fire, and the
    // drain check would tear the stream down mid-fade.
    assert_eq!(
        fade_len_samples(per_second, 6, per_second * 2),
        Some((per_second * 2) as usize)
    );
    // Exactly the setting is not "late".
    assert_eq!(
        fade_len_samples(per_second, 6, per_second * 6),
        Some((per_second * 6) as usize)
    );
}

#[test]
fn a_fade_is_refused_rather_than_made_absurdly_short() {
    let per_second = 44_100u64 * 2;
    // Under a second there is nothing worth hearing; the hard cut is cleaner.
    assert_eq!(fade_len_samples(per_second, 6, per_second - 1), None);
    assert_eq!(fade_len_samples(per_second, 6, 0), None);
    // Exactly one second is still a fade.
    assert!(fade_len_samples(per_second, 6, per_second).is_some());
    // Degenerate inputs refuse rather than divide or overflow.
    assert_eq!(fade_len_samples(0, 6, per_second * 10), None);
    assert_eq!(fade_len_samples(per_second, 0, per_second * 10), None);
}

#[test]
fn mix_reuses_caller_scratch_and_never_allocates_per_call() {
    // The callback is a real-time context. `mix_frames` must write into buffers the
    // caller already owns; this pins that shape against a later refactor quietly
    // returning a fresh Vec.
    let mut scratch_out = vec![0.0f32; 128];
    let mut scratch_in = vec![0.0f32; 128];
    let mut dst = vec![0.0f32; 128];
    scratch_out.fill(0.5);
    scratch_in.fill(0.5);
    let mut pos = 0usize;
    for _ in 0..8 {
        pos = mix_frames(&mut dst, &scratch_out, &scratch_in, pos, 1024, 1.0);
    }
    assert_eq!(pos, 1024);
    assert_eq!(dst.len(), 128, "mix must not resize the destination");
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

/// A pass whose ring gave nothing hands the mixer an empty slice, and the padding has to
/// land where a single pass over a source that simply ran short would have put it.
/// Otherwise a starved ring would sound different depending on the quantum, which is the
/// one way the pass split could become audible.
#[test]
fn a_pass_that_drained_nothing_pads_like_a_source_that_ran_short() {
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
/// `pinned` is now a plain argument; both answers are asserted here on every host.
#[test]
fn a_differing_rate_is_refused_only_where_the_engine_rate_moves() {
    assert!(
        crossfade_accepts_rate(96_000, 44_100, true),
        "a pinned engine rate is exactly the condition that makes this safe"
    );
    assert!(
        !crossfade_accepts_rate(96_000, 44_100, false),
        "an unpinned engine rate still refuses a differing native rate"
    );
}

/// The equal-rate case has to hold everywhere, or the feature dies on Linux.
#[test]
fn a_matching_rate_is_always_accepted() {
    assert!(crossfade_accepts_rate(44_100, 44_100, true));
    assert!(crossfade_accepts_rate(44_100, 44_100, false));
    assert!(crossfade_accepts_rate(96_000, 96_000, true));
    assert!(crossfade_accepts_rate(96_000, 96_000, false));
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

/// The refit is for a fade that cannot be honoured, never for one that can: rewriting a
/// healthy fade would shorten every overlap that merely drained a little unevenly.
#[test]
fn a_fade_with_audio_to_spare_is_left_alone() {
    assert!(refit_fade(400, 1000, 600).is_none());
    assert!(refit_fade(400, 1000, 5000).is_none());
    assert!(
        refit_fade(1000, 1000, 0).is_none(),
        "a finished fade needs no refit"
    );
}

/// An incoming track with nothing left at all is left to the mixer, not refitted.
///
/// Forcing the position to `len` here looks like "end the fade", but it drops the
/// OUTGOING envelope from cos(N/len) to zero in a single callback (the gain step this
/// function exists to avoid), and the swap still waits on the outgoing track's own end,
/// both sides sit at silence until it arrives. The mixer already degrades this case
/// to a plain fade-out by contributing exact zeros.
#[test]
fn a_fade_with_no_audio_left_is_not_refitted() {
    assert_eq!(refit_fade(400, 1000, 0), None);
}
