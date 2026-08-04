//! Tests for `src/player/dash.rs`, attached to it by `#[path]`.
//!
//! Synthetic manifests rather than fixtures: the repo keeps no `.mpd` files, and every
//! shape below is a spec construct we must handle whatever TIDAL happens to emit today.
//! `timescale="44100"` throughout, so `d="441000"` is exactly ten seconds.

use super::*;

/// Wraps a Period body in the smallest MPD the parser accepts.
fn mpd(mpd_attrs: &str, period: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" profiles="urn:mpeg:dash:profile:isoff-on-demand:2011" type="static" minBufferTime="PT2S"{mpd_attrs}>
  {period}
</MPD>"#
    )
}

/// Wraps a SegmentTemplate body in a Period/AdaptationSet/Representation chain.
fn period(period_attrs: &str, seg_tpl: &str) -> String {
    format!(
        r#"<Period{period_attrs}>
    <AdaptationSet mimeType="audio/mp4">
      <Representation id="1" codecs="mp4a.40.2" audioSamplingRate="44100" bandwidth="128000">
        {seg_tpl}
      </Representation>
    </AdaptationSet>
  </Period>"#
    )
}

const TPL_HEAD: &str = r#"<SegmentTemplate timescale="44100" initialization="init.mp4" media="seg$Number$.mp4" startNumber="1""#;

#[test]
fn positive_repeat_count_is_unchanged() {
    // Regression guard on the path that already worked: r="2" means three segments.
    let xml = mpd(
        r#" mediaPresentationDuration="PT30S""#,
        &period(
            "",
            &format!(
                r#"{TPL_HEAD}>
          <SegmentTimeline><S d="441000" r="2" /></SegmentTimeline>
        </SegmentTemplate>"#
            ),
        ),
    );
    let m = parse_dash_mpd(&xml).expect("positive @r must parse");
    assert_eq!(m.segment_urls.len(), 3);
    assert_eq!(m.segment_urls[0], "seg1.mp4");
    assert_eq!(m.segment_urls[2], "seg3.mp4");
}

#[test]
fn negative_repeat_resolves_against_period_duration() {
    // Both defects at once: @r="-1", which used to clamp to a single segment, bounded by
    // Period@duration, which used not to be consulted at all.
    let xml = mpd(
        "",
        &period(
            r#" duration="PT30S""#,
            &format!(
                r#"{TPL_HEAD}>
          <SegmentTimeline><S t="0" d="441000" r="-1" /></SegmentTimeline>
        </SegmentTemplate>"#
            ),
        ),
    );
    let m = parse_dash_mpd(&xml).expect("negative @r with a Period duration must parse");
    assert_eq!(m.segment_urls.len(), 3, "30s / 10s per segment");
    assert_eq!(m.duration_secs, Some(30.0), "resolved from Period@duration");
}

#[test]
fn a_periods_own_duration_bounds_its_timeline_not_the_presentation_total() {
    // Both sources present and disagreeing, which no other test covers. A 30s first Period
    // inside a 60s presentation must yield three segments; bounding by the MPD total runs the
    // enumeration through Periods this parser never even reads, and produced six.
    let xml = mpd(
        r#" mediaPresentationDuration="PT60S""#,
        &period(
            r#" duration="PT30S""#,
            &format!(
                r#"{TPL_HEAD}>
          <SegmentTimeline><S t="0" d="441000" r="-1" /></SegmentTimeline>
        </SegmentTemplate>"#
            ),
        ),
    );
    let m = parse_dash_mpd(&xml).expect("both duration sources present must parse");
    assert_eq!(m.segment_urls.len(), 3, "30s of Period, not 60s of MPD");
    assert_eq!(
        m.duration_secs,
        Some(30.0),
        "the reported duration is the Period we actually enumerated"
    );
}

#[test]
fn a_start_number_at_the_ceiling_is_refused_not_wrapped() {
    // @startNumber is an unbounded xs:unsignedLong, so the *second* segment already walks off
    // u64, nowhere near MAX_SEGMENTS: naked addition panicked under the dev profile's overflow
    // checks and wrapped $Number$ to 0 in release. Saturating instead would render the same
    // number for every slot past the ceiling, a list of duplicate URLs sold as a whole track.
    const CEILING: &str = "18446744073709551615";

    for seg_tpl in [
        format!(
            r#"<SegmentTemplate timescale="44100" initialization="init.mp4" media="seg$Number$.mp4" startNumber="{CEILING}">
          <SegmentTimeline><S d="441000" r="2" /></SegmentTimeline>
        </SegmentTemplate>"#
        ),
        format!(
            r#"<SegmentTemplate timescale="44100" duration="441000" initialization="init.mp4" media="seg$Number$.mp4" startNumber="{CEILING}" />"#
        ),
    ] {
        let xml = mpd(
            r#" mediaPresentationDuration="PT30S""#,
            &period("", &seg_tpl),
        );
        let err = match parse_dash_mpd(&xml) {
            Ok(_) => panic!("a startNumber at the u64 ceiling must be refused: {seg_tpl}"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("startNumber"), "{err}");
    }
}

#[test]
fn a_partial_representation_template_still_inherits_the_adaptation_sets_offset() {
    // ISO/IEC 23009-1 5.3.9.1 resolves SegmentTemplate attribute by attribute, lowest level
    // first. Picking whichever element sat lowest dropped @presentationTimeOffset outright
    // here, leaving a bound of 30 against a cursor at 100: zero repeats, and the fail-closed
    // check then rejected a manifest that plays.
    let xml = mpd(
        "",
        r#"<Period duration="PT30S">
    <AdaptationSet mimeType="audio/mp4">
      <SegmentTemplate timescale="1" presentationTimeOffset="100" />
      <Representation id="1" codecs="mp4a.40.2" audioSamplingRate="44100" bandwidth="128000">
        <SegmentTemplate initialization="init.mp4" media="seg$Number$.mp4" startNumber="1">
          <SegmentTimeline><S t="100" d="10" r="-1" /></SegmentTimeline>
        </SegmentTemplate>
      </Representation>
    </AdaptationSet>
  </Period>"#,
    );
    let m = parse_dash_mpd(&xml).expect("a split SegmentTemplate must resolve per attribute");
    assert_eq!(
        m.segment_urls.len(),
        3,
        "(100 + 30) - 100, over 10 per segment"
    );
}

#[test]
fn a_nonzero_presentation_time_offset_anchors_the_bound() {
    // S@t is measured from the offset, so the Period's end in this tick space is offset plus
    // duration. Reading the bound as the duration alone made saturating_sub yield zero
    // repeats, and the fail-closed check then rejected a manifest that should play.
    let xml = mpd(
        "",
        &period(
            r#" duration="PT30S""#,
            r#"<SegmentTemplate timescale="1" presentationTimeOffset="100" initialization="init.mp4" media="seg$Number$.mp4" startNumber="1">
          <SegmentTimeline><S t="100" d="10" r="-1" /></SegmentTimeline>
        </SegmentTemplate>"#,
        ),
    );
    let m = parse_dash_mpd(&xml).expect("a nonzero presentationTimeOffset must parse");
    assert_eq!(
        m.segment_urls.len(),
        3,
        "(100 + 30) - 100, over 10 per segment"
    );
}

#[test]
fn an_absent_first_start_time_defaults_to_the_offset_not_to_zero() {
    // S@t is optional on the first element and means "at the Period start", which is the
    // offset. Starting the cursor at zero against an offset-anchored bound inflated the count
    // by offset/@d: 13 segments here instead of 3.
    let xml = mpd(
        "",
        &period(
            r#" duration="PT30S""#,
            r#"<SegmentTemplate timescale="1" presentationTimeOffset="100" initialization="init.mp4" media="seg$Number$.mp4" startNumber="1">
          <SegmentTimeline><S d="10" r="-1" /></SegmentTimeline>
        </SegmentTemplate>"#,
        ),
    );
    let m = parse_dash_mpd(&xml).expect("an absent first @t must parse");
    assert_eq!(m.segment_urls.len(), 3);
}

#[test]
fn negative_repeat_stops_at_the_next_segment_start_time() {
    // With a following S element, the open-ended run ends at that element's @t, not at
    // the end of the period.
    let xml = mpd(
        r#" mediaPresentationDuration="PT40S""#,
        &period(
            "",
            &format!(
                r#"{TPL_HEAD}>
          <SegmentTimeline>
            <S t="0" d="441000" r="-1" />
            <S t="1323000" d="441000" />
          </SegmentTimeline>
        </SegmentTemplate>"#
            ),
        ),
    );
    let m = parse_dash_mpd(&xml).expect("bounded negative @r must parse");
    assert_eq!(
        m.segment_urls.len(),
        4,
        "3 from the open run, then 1 explicit"
    );
    assert_eq!(m.segment_urls[3], "seg4.mp4");
}

#[test]
fn duration_mode_resolves_against_the_period_duration() {
    // @duration mode with no MPD@mediaPresentationDuration produced an empty list, which
    // the caller then read as a successfully parsed manifest.
    let xml = mpd(
        "",
        &period(
            r#" duration="PT30S""#,
            &format!(r#"{TPL_HEAD} duration="441000" />"#),
        ),
    );
    let m = parse_dash_mpd(&xml).expect("@duration with a Period duration must parse");
    assert_eq!(m.segment_urls.len(), 3);
}

#[test]
fn no_resolvable_duration_fails_closed() {
    // The invariant: never an Ok carrying an empty segment list.
    let xml = mpd(
        "",
        &period("", &format!(r#"{TPL_HEAD} duration="441000" />"#)),
    );
    let err = parse_dash_mpd(&xml).expect_err("no duration source must be an error");
    assert!(
        err.to_string().contains("no segments"),
        "unexpected message: {err}"
    );
}

#[test]
fn unbounded_negative_repeat_fails_closed() {
    // A trailing @r="-1" with nothing to bound it is equally unresolvable; emitting the
    // single segment the old clamp produced is what made a truncated track look normal.
    let xml = mpd(
        "",
        &period(
            "",
            &format!(
                r#"{TPL_HEAD}>
          <SegmentTimeline><S t="0" d="441000" r="-1" /></SegmentTimeline>
        </SegmentTemplate>"#
            ),
        ),
    );
    let err = parse_dash_mpd(&xml).expect_err("unbounded negative @r must be an error");
    assert!(
        err.to_string().contains("no segments"),
        "unexpected message: {err}"
    );
}

#[test]
fn end_number_caps_both_branches() {
    let timeline = mpd(
        r#" mediaPresentationDuration="PT100S""#,
        &period(
            "",
            &format!(
                r#"{TPL_HEAD} endNumber="3">
          <SegmentTimeline><S d="441000" r="9" /></SegmentTimeline>
        </SegmentTemplate>"#
            ),
        ),
    );
    let m = parse_dash_mpd(&timeline).expect("timeline with endNumber must parse");
    assert_eq!(m.segment_urls.len(), 3, "startNumber=1, endNumber=3");

    let duration_mode = mpd(
        r#" mediaPresentationDuration="PT100S""#,
        &period(
            "",
            &format!(r#"{TPL_HEAD} endNumber="4" duration="441000" />"#),
        ),
    );
    let m = parse_dash_mpd(&duration_mode).expect("@duration with endNumber must parse");
    assert_eq!(m.segment_urls.len(), 4);
}

#[test]
fn an_absurd_declared_duration_is_refused_before_it_allocates() {
    // P100Y over one-tick segments is schema-valid and expands to ~1e14 pushes. The cap has
    // to reject rather than truncate: a quietly short list is the defect this parser was
    // fixed to stop producing.
    let xml = mpd(
        r#" mediaPresentationDuration="P100Y""#,
        &period(
            "",
            r#"<SegmentTemplate timescale="90000" initialization="init.mp4" media="seg$Number$.mp4" startNumber="1" duration="1" />"#,
        ),
    );
    let err = parse_dash_mpd(&xml).expect_err("an unbounded expansion must be refused");
    assert!(
        err.to_string().contains("expands past"),
        "unexpected message: {err}"
    );
}

#[test]
fn an_absurd_repeat_count_is_refused_before_it_allocates() {
    // @r is an xs:integer with no upper facet, so i64::MAX is a legal manifest value.
    let xml = mpd(
        r#" mediaPresentationDuration="PT30S""#,
        &period(
            "",
            &format!(
                r#"{TPL_HEAD}>
          <SegmentTimeline><S d="441000" r="9223372036854775807" /></SegmentTimeline>
        </SegmentTemplate>"#
            ),
        ),
    );
    let err = parse_dash_mpd(&xml).expect_err("an i64::MAX repeat must be refused");
    assert!(
        err.to_string().contains("expands past"),
        "unexpected message: {err}"
    );
}

#[test]
fn a_long_but_plausible_track_stays_under_the_cap() {
    // Six hours at ten seconds a segment is 2 160 URLs: comfortably inside the bound, so the
    // guard cannot be mistaken for one that rejects real long-form audio.
    let xml = mpd(
        r#" mediaPresentationDuration="PT6H""#,
        &period("", &format!(r#"{TPL_HEAD} duration="441000" />"#)),
    );
    let m = parse_dash_mpd(&xml).expect("a six-hour track must still parse");
    assert_eq!(m.segment_urls.len(), 2160);
}

#[test]
fn zero_timescale_does_not_divide_by_zero() {
    // timescale="0" is invalid; it must fall back to the spec default rather than
    // producing an infinite segment count.
    let xml = mpd(
        r#" mediaPresentationDuration="PT3S""#,
        &period(
            "",
            r#"<SegmentTemplate timescale="0" initialization="init.mp4" media="seg$Number$.mp4" startNumber="1" duration="1" />"#,
        ),
    );
    let m = parse_dash_mpd(&xml).expect("a zero timescale must not panic or hang");
    assert_eq!(m.segment_urls.len(), 3);
}
