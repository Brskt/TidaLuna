//! Tests for `src/player/dash.rs`, attached to it by `#[path]`.
//!
//! Synthetic manifests rather than fixtures: the repo keeps no `.mpd` files, and every
//! shape below is a spec construct we must handle whatever TIDAL happens to emit today.
//! `timescale="44100"` throughout: `d="441000"` is exactly ten seconds.

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

/// A `TPL_HEAD` template carrying `attrs` and a SegmentTimeline of `segments`.
fn timeline_tpl(attrs: &str, segments: &str) -> String {
    format!(
        r#"{TPL_HEAD}{attrs}>
          <SegmentTimeline>{segments}</SegmentTimeline>
        </SegmentTemplate>"#
    )
}

/// `count` segments, the `duration` and the `named` urls when a row states them. Every row is
/// plain AAC-LC, the 320 kbps tier the profile check must let through.
#[test]
fn a_manifest_resolves_to_exactly_the_segments_it_describes() {
    let duration_tpl = format!(r#"{TPL_HEAD} duration="441000" />"#);
    type Row<'a> = (&'a str, String, usize, Option<f64>, &'a [(usize, &'a str)]);
    let rows: [Row; 12] = [
        (
            "a positive @r is unchanged: r=\"2\" means three segments",
            mpd(
                r#" mediaPresentationDuration="PT30S""#,
                &period("", &timeline_tpl("", r#"<S d="441000" r="2" />"#)),
            ),
            3,
            None,
            &[(0, "seg1.mp4"), (2, "seg3.mp4")],
        ),
        (
            // @r="-1" used to clamp to one segment, and Period@duration was not consulted at all.
            "a negative @r resolves against the Period duration",
            mpd(
                "",
                &period(
                    r#" duration="PT30S""#,
                    &timeline_tpl("", r#"<S t="0" d="441000" r="-1" />"#),
                ),
            ),
            3,
            Some(30.0),
            &[],
        ),
        (
            // Bounding by the MPD total ran the enumeration through Periods never read: six.
            "a Period's own duration bounds its timeline, not the presentation total",
            mpd(
                r#" mediaPresentationDuration="PT60S""#,
                &period(
                    r#" duration="PT30S""#,
                    &timeline_tpl("", r#"<S t="0" d="441000" r="-1" />"#),
                ),
            ),
            3,
            Some(30.0),
            &[],
        ),
        (
            // ISO/IEC 23009-1 5.3.9.1 resolves SegmentTemplate attribute by attribute: dropping
            // the AdaptationSet's offset left a bound of 30 against a cursor at 100.
            "a partial Representation template still inherits the AdaptationSet's offset",
            mpd(
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
            ),
            3,
            None,
            &[],
        ),
        (
            // S@t is measured from the offset: reading the bound as the duration alone gave zero
            // repeats, and the fail-closed check rejected a manifest that plays.
            "a nonzero presentationTimeOffset anchors the bound",
            mpd(
                "",
                &period(
                    r#" duration="PT30S""#,
                    r#"<SegmentTemplate timescale="1" presentationTimeOffset="100" initialization="init.mp4" media="seg$Number$.mp4" startNumber="1">
          <SegmentTimeline><S t="100" d="10" r="-1" /></SegmentTimeline>
        </SegmentTemplate>"#,
                ),
            ),
            3,
            None,
            &[],
        ),
        (
            // An absent first S@t means the Period start, which is the offset: starting at zero
            // inflated the count by offset/@d (13 segments instead of 3).
            "an absent first start time defaults to the offset, not to zero",
            mpd(
                "",
                &period(
                    r#" duration="PT30S""#,
                    r#"<SegmentTemplate timescale="1" presentationTimeOffset="100" initialization="init.mp4" media="seg$Number$.mp4" startNumber="1">
          <SegmentTimeline><S d="10" r="-1" /></SegmentTimeline>
        </SegmentTemplate>"#,
                ),
            ),
            3,
            None,
            &[],
        ),
        (
            "a negative @r stops at the next segment start time",
            mpd(
                r#" mediaPresentationDuration="PT40S""#,
                &period(
                    "",
                    &timeline_tpl(
                        "",
                        r#"<S t="0" d="441000" r="-1" /><S t="1323000" d="441000" />"#,
                    ),
                ),
            ),
            4,
            None,
            &[(3, "seg4.mp4")],
        ),
        (
            // @duration mode with no MPD duration produced an empty list read as a success.
            "duration mode resolves against the Period duration",
            mpd("", &period(r#" duration="PT30S""#, &duration_tpl)),
            3,
            None,
            &[],
        ),
        (
            "endNumber caps the timeline branch",
            mpd(
                r#" mediaPresentationDuration="PT100S""#,
                &period(
                    "",
                    &timeline_tpl(r#" endNumber="3""#, r#"<S d="441000" r="9" />"#),
                ),
            ),
            3,
            None,
            &[],
        ),
        (
            "endNumber caps the duration branch",
            mpd(
                r#" mediaPresentationDuration="PT100S""#,
                &period(
                    "",
                    &format!(r#"{TPL_HEAD} endNumber="4" duration="441000" />"#),
                ),
            ),
            4,
            None,
            &[],
        ),
        (
            // Six hours at ten seconds a segment: the guard cannot be mistaken for one that
            // rejects real long-form audio.
            "a long but plausible track stays under the cap",
            mpd(
                r#" mediaPresentationDuration="PT6H""#,
                &period("", &duration_tpl),
            ),
            2160,
            None,
            &[],
        ),
        (
            "a zero timescale falls back to the spec default instead of an infinite count",
            mpd(
                r#" mediaPresentationDuration="PT3S""#,
                &period(
                    "",
                    r#"<SegmentTemplate timescale="0" initialization="init.mp4" media="seg$Number$.mp4" startNumber="1" duration="1" />"#,
                ),
            ),
            3,
            None,
            &[],
        ),
    ];
    for (what, xml, count, duration, named) in rows {
        let m = parse_dash_mpd(&xml).unwrap_or_else(|e| panic!("{what}: {e}"));
        assert_eq!(m.segment_urls.len(), count, "{what}");
        if let Some(secs) = duration {
            assert_eq!(m.duration_secs, Some(secs), "{what}");
        }
        for &(at, url) in named {
            assert_eq!(m.segment_urls[at], url, "{what}");
        }
        assert_eq!(m.codec, "mp4a.40.2", "{what}");
    }
}

/// A manifest that cannot be enumerated in full is refused, never truncated: a quietly short list
/// is a truncated track sold as a whole one. The text names the guard that fired; the wrong
/// ceiling cannot pass.
#[test]
fn a_manifest_that_cannot_be_enumerated_in_full_is_refused() {
    // @startNumber is an unbounded xs:unsignedLong; the second segment already walks off u64.
    // Saturating would render one number for every slot past it, duplicate URLs as a whole track.
    const CEILING: &str = "18446744073709551615";
    let padding = "p".repeat(MAX_SEGMENT_BYTES / 32);
    let rows = [
        (
            "a startNumber at the ceiling, timeline",
            mpd(
                r#" mediaPresentationDuration="PT30S""#,
                &period(
                    "",
                    &format!(
                        r#"<SegmentTemplate timescale="44100" initialization="init.mp4" media="seg$Number$.mp4" startNumber="{CEILING}">
          <SegmentTimeline><S d="441000" r="2" /></SegmentTimeline>
        </SegmentTemplate>"#
                    ),
                ),
            ),
            "startNumber",
        ),
        (
            "a startNumber at the ceiling, duration",
            mpd(
                r#" mediaPresentationDuration="PT30S""#,
                &period(
                    "",
                    &format!(
                        r#"<SegmentTemplate timescale="44100" duration="441000" initialization="init.mp4" media="seg$Number$.mp4" startNumber="{CEILING}" />"#
                    ),
                ),
            ),
            "startNumber",
        ),
        (
            // The invariant: never an Ok carrying an empty segment list.
            "no resolvable duration",
            mpd(
                "",
                &period("", &format!(r#"{TPL_HEAD} duration="441000" />"#)),
            ),
            "no segments",
        ),
        (
            "an unbounded negative @r",
            mpd(
                "",
                &period("", &timeline_tpl("", r#"<S t="0" d="441000" r="-1" />"#)),
            ),
            "no segments",
        ),
        (
            // P100Y over one-tick segments is schema-valid and expands to hundreds of trillions of pushes.
            "an absurd declared duration",
            mpd(
                r#" mediaPresentationDuration="P100Y""#,
                &period(
                    "",
                    r#"<SegmentTemplate timescale="90000" initialization="init.mp4" media="seg$Number$.mp4" startNumber="1" duration="1" />"#,
                ),
            ),
            "segments; refusing",
        ),
        (
            // 40 segments, each URL over a 32nd of the ceiling: past it whatever the ceiling is.
            "an amplified template with a small count",
            mpd(
                r#" mediaPresentationDuration="PT40S""#,
                &period(
                    "",
                    &format!(
                        r#"<SegmentTemplate timescale="1" initialization="init.mp4" media="{padding}seg$Number$.mp4" startNumber="1" duration="1" />"#
                    ),
                ),
            ),
            "bytes of segment urls",
        ),
        (
            // @r is an xs:integer with no upper facet; i64::MAX is a legal manifest value.
            "an absurd repeat count",
            mpd(
                r#" mediaPresentationDuration="PT30S""#,
                &period(
                    "",
                    &timeline_tpl("", r#"<S d="441000" r="9223372036854775807" />"#),
                ),
            ),
            "segments; refusing",
        ),
    ];
    for (what, xml, expected) in rows {
        let err = parse_dash_mpd(&xml).expect_err(what);
        assert!(err.to_string().contains(expected), "{what}: {err}");
    }
}

/// TIDAL's 96 kbps tier serves HE-AAC, which symphonia rejects at codec init, once the whole track
/// has been fetched and a device opened. The manifest names the profile and the refusal belongs
/// here, typed: the renderer only sees the IPC failure code, and prose would break on a rewording.
#[test]
fn an_undecodable_aac_profile_is_refused_with_its_own_type_before_anything_downloads() {
    for codec in ["mp4a.40.5", "mp4a.40.29"] {
        let xml = mpd(
            r#" mediaPresentationDuration="PT30S""#,
            &format!(
                r#"<Period>
    <AdaptationSet mimeType="audio/mp4">
      <Representation id="1" codecs="{codec}" audioSamplingRate="44100" bandwidth="96000">
        {TPL_HEAD} duration="441000" />
      </Representation>
    </AdaptationSet>
  </Period>"#
            ),
        );
        let err = match parse_dash_mpd(&xml) {
            Ok(_) => panic!("an undecodable AAC profile must be refused: {codec}"),
            Err(e) => e,
        };
        assert!(err.to_string().contains(codec), "{err}");
        // This text is for the log. What a listener is told (which quality to pick) is the
        // renderer's wording, reached through the failure code rather than this string.
        assert!(
            !err.to_string().contains("320"),
            "the listener-facing advice belongs to the renderer, not the log: {err}"
        );
        let typed = err
            .downcast_ref::<UndecodableProfile>()
            .expect("the refusal must carry its own type, not just a message");
        assert_eq!(typed.codec, codec);
    }

    // A malformed manifest must NOT wear that type, or every parse failure would be reported
    // to the user as a quality problem.
    let broken = mpd(
        "",
        &period("", &format!(r#"{TPL_HEAD} duration="441000" />"#)),
    );
    let other = parse_dash_mpd(&broken).expect_err("no duration source must still be an error");
    assert!(
        other.downcast_ref::<UndecodableProfile>().is_none(),
        "an unrelated parse failure must not be typed as a profile refusal: {other}"
    );
}
