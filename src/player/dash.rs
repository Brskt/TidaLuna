use anyhow::Result;
use serde::{Deserialize, Serialize};

/// Decoded DASH manifest with segment URLs extracted from MPD XML.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DashManifest {
    pub init_url: String,
    pub segment_urls: Vec<String>,
    pub codec: String,
    #[serde(default)]
    pub sample_rate: Option<u32>,
    #[serde(default)]
    pub bandwidth: Option<u32>,
    #[serde(default)]
    pub duration_secs: Option<f64>,
}

/// Hard ceiling on how many segments one manifest may expand to.
///
/// ISO/IEC 23009-1 bounds none of the inputs. `@r` is an `xs:integer` with no `maxInclusive`
/// facet (clause 5.3.9.6.3) and `@mediaPresentationDuration` an `xs:duration` with an
/// unbounded value space: `mediaPresentationDuration="P100Y"` over one-tick segments is a
/// schema-valid manifest that demands more allocations than the process can survive. Neither
/// ingress is ours. `player.parse_dash` is reachable from any script in the TIDAL frame, which
/// is where plugin code runs. `apply_dash_manifest` parses what the Connect receiver fetched
/// over HTTPS from a `*.tidal.com` host for a media id a LAN peer chose; those bytes are
/// TIDAL's answer rather than the peer's, a weaker path this parser still cannot tell apart.
///
/// The figure covers 24 hours at the shortest segment length in common use (2s yields
/// 43 200; 10s yields 8 640), then rounds up. Shaka Player is the precedent for capping the
/// count rather than the declared duration, at a far tighter default
/// (`dash.initialSegmentLimit`, 1000); ExoPlayer, GPAC and ffmpeg's `dashdec` cap nothing.
const MAX_SEGMENTS: usize = 50_000;

/// Emit one segment URL, refusing to go past [`MAX_SEGMENTS`].
///
/// Enforced per push rather than against a count computed up front, the count itself being
/// the hostile value: an `@r` of `i64::MAX` has to be rejected without ever reserving for it.
/// Refusing beats truncating, a quietly short list being the failure this parser exists to
/// stop producing.
fn push_segment(urls: &mut Vec<String>, media_tpl: &str, number: u64) -> Result<()> {
    if urls.len() >= MAX_SEGMENTS {
        anyhow::bail!("DASH manifest expands past {MAX_SEGMENTS} segments; refusing it");
    }
    urls.push(media_tpl.replace("$Number$", &number.to_string()));
    Ok(())
}

/// The value of one `SegmentTemplate` attribute in force for a Representation, resolved
/// lowest level first over `levels`.
///
/// Per ISO/IEC 23009-1 clause 5.3.9.1, `SegmentTemplate` "shall inherit attributes and
/// elements from the same element on a higher level. If the same attribute or element is
/// present on both levels, the one on the lower level shall take precedence over the one on
/// the higher level." Per attribute, then, across Period, AdaptationSet and Representation;
/// picking whichever element sits lowest instead drops every attribute an ancestor supplies
/// and the closer template omits. Shaka Player, ExoPlayer and GPAC all cascade this way,
/// Period level included.
fn inherited<'a, T>(
    levels: [Option<&'a dash_mpd::SegmentTemplate>; 3],
    pick: impl Fn(&'a dash_mpd::SegmentTemplate) -> Option<T>,
) -> Option<T> {
    levels.into_iter().flatten().find_map(pick)
}

/// The segment number `offset` slots into the enumeration, refusing a `@startNumber` that
/// walks off `u64`.
///
/// The attribute is an unbounded `xs:unsignedLong` the manifest chooses; a value near the
/// ceiling overflows on the *second* segment, nowhere near [`MAX_SEGMENTS`]. Saturating would
/// be worse than refusing; every slot past the ceiling renders the same `$Number$`, a list of
/// duplicate URLs passed off as a whole track.
fn segment_number(start_number: u64, offset: u64) -> Result<u64> {
    start_number.checked_add(offset).ok_or_else(|| {
        anyhow::anyhow!("DASH manifest declares a startNumber that overflows at segment {offset}")
    })
}

/// Duration in seconds of the Period whose segments we are about to enumerate.
///
/// `Period@duration` first, `MPD@mediaPresentationDuration` only as a fallback: the two
/// measure different things. Per ISO/IEC 23009-1 5.3.2 a Period's length is its own
/// `@duration` (or the next Period's `@start`), while the MPD attribute spans the whole
/// presentation. Bounding one Period's timeline by the presentation total runs the
/// enumeration through the Periods that follow, and we only ever read the first.
///
/// Absent both, the count is not computable from the manifest and the caller fails closed.
fn period_duration_secs(mpd: &dash_mpd::MPD, period: &dash_mpd::Period) -> Option<f64> {
    period
        .duration
        .as_ref()
        .or(mpd.mediaPresentationDuration.as_ref())
        .map(|d| d.as_secs_f64())
}

/// MPEG-4 Audio Object Type 2, plain AAC-LC: the only profile this build decodes.
const AAC_LC_OBJECT_TYPE: u16 = 2;

/// A manifest naming an audio profile no decoder in this build can handle.
///
/// A type rather than a bare message, because two different callers need to tell it apart
/// from a malformed manifest: the IPC layer gives it a failure code of its own, and the
/// renderer picks what to show the user from that code. Matching on the prose would drop the
/// user's message the first time someone rewords it, and say nothing when it did.
#[derive(Debug)]
pub struct UndecodableProfile {
    pub codec: String,
}

impl std::fmt::Display for UndecodableProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "this stream is {}, which TidaLunar cannot decode: only AAC-LC is supported",
            self.codec
        )
    }
}

impl std::error::Error for UndecodableProfile {}

/// The MPEG-4 Audio Object Type a codec string names, if it names one.
///
/// Per RFC 6381 clause 3.3 an `mp4a` codec carries the ObjectTypeIndication in hex and, for
/// `40` (MPEG-4 Audio), the Audio Object Type after it in decimal. Any other family, or a
/// truncated `mp4a.40`, yields `None`: this parser only speaks for the profiles it can name,
/// and leaves the rest to the decoder.
fn mp4a_object_type(codec: &str) -> Option<u16> {
    codec
        .strip_prefix("mp4a.40.")
        .and_then(|aot| aot.parse().ok())
}

/// Parse a DASH MPD XML string and extract segment URLs.
///
/// Fails closed: a manifest that parses but yields no segments is an error, not an empty
/// `Ok`. No caller knows more than this function about whether the list is complete, and none
/// can tell a legitimately short track from an unresolvable duration or repeat count.
pub fn parse_dash_mpd(xml: &str) -> Result<DashManifest> {
    // TIDAL uses group="main" (string) which violates the DASH spec (expects integer).
    // Remove non-standard attributes before parsing.
    let cleaned = xml.replace(r#" group="main""#, "");
    let mpd = dash_mpd::parse(&cleaned).map_err(|e| anyhow::anyhow!("Failed to parse MPD: {e}"))?;

    let period = mpd
        .periods
        .first()
        .ok_or_else(|| anyhow::anyhow!("MPD has no periods"))?;

    let adaptation = period
        .adaptations
        .first()
        .ok_or_else(|| anyhow::anyhow!("MPD period has no adaptation sets"))?;

    let repr = adaptation
        .representations
        .first()
        .ok_or_else(|| anyhow::anyhow!("MPD adaptation set has no representations"))?;

    let codec = repr.codecs.clone().unwrap_or_default();
    // `symphonia-codec-aac` decodes AAC-LC and nothing else: it refuses every other object
    // type, and SBR besides, at codec init. That refusal used to land after the whole track
    // had been fetched and an output device opened, so the tier played silence. TIDAL serves
    // HE-AAC (object type 5) at 96 kbps and the manifest says so right here, before a single
    // segment is pulled.
    if let Some(aot) = mp4a_object_type(&codec)
        && aot != AAC_LC_OBJECT_TYPE
    {
        return Err(UndecodableProfile { codec }.into());
    }
    let sample_rate = repr
        .audioSamplingRate
        .as_deref()
        .and_then(|s| s.parse::<u32>().ok());
    let bandwidth = repr.bandwidth.map(|b| b as u32);

    // Lowest level first, which is the precedence order [`inherited`] walks.
    let levels = [
        repr.SegmentTemplate.as_ref(),
        adaptation.SegmentTemplate.as_ref(),
        period.SegmentTemplate.as_ref(),
    ];
    if levels.iter().all(Option::is_none) {
        anyhow::bail!("No SegmentTemplate found in MPD");
    }

    let init_url = inherited(levels, |t| t.initialization.clone())
        .ok_or_else(|| anyhow::anyhow!("SegmentTemplate has no initialization URL"))?;

    let media_tpl = inherited(levels, |t| t.media.clone())
        .ok_or_else(|| anyhow::anyhow!("SegmentTemplate has no media URL template"))?;

    let start_number = inherited(levels, |t| t.startNumber).unwrap_or(1);
    let end_number = inherited(levels, |t| t.endNumber);
    // A zero timescale would divide by zero below; the spec's default is 1.
    let timescale = inherited(levels, |t| t.timescale)
        .filter(|t| *t > 0)
        .unwrap_or(1) as f64;
    let total_secs = period_duration_secs(&mpd, period);
    // Origin of this Period's timeline, in the tick space `S@t` uses. Per ISO/IEC 23009-1
    // 5.3.9.6, `S@t` minus `@presentationTimeOffset` is the start relative to the Period, so
    // cursor and end bound both have to be anchored here or they straddle two origins.
    let pto = inherited(levels, |t| t.presentationTimeOffset).unwrap_or(0);
    let seg_timeline = inherited(levels, |t| t.SegmentTimeline.as_ref());
    let seg_duration = inherited(levels, |t| t.duration);

    let mut segment_urls = Vec::new();
    if let Some(timeline) = seg_timeline {
        // Converting the bound once keeps every comparison below in ticks.
        let total_ticks = total_secs.map(|s| pto.saturating_add((s * timescale) as u64));
        // A slot index, not a running number. Deriving the number per slot catches a hostile
        // `@startNumber` before it can wrap; `MAX_SEGMENTS`, enforced by `push_segment` ahead
        // of every increment, bounds the index itself.
        let mut offset: u64 = 0;
        // The origin, not zero: `S@t` is optional on the first element and then means "at the
        // Period start", which here is `pto`.
        let mut cursor = pto;
        for (i, s) in timeline.segments.iter().enumerate() {
            if let Some(t) = s.t {
                cursor = t;
            }
            let repeat = match s.r {
                // ISO/IEC 23009-1 5.3.9.6.1: a negative @r repeats this duration until the
                // next S element's @t, the end of the Period, or the next MPD update. Not
                // once; clamping it to a single segment truncated the track without a word.
                Some(r) if r < 0 => {
                    let end = timeline
                        .segments
                        .get(i + 1)
                        .and_then(|next| next.t)
                        .or(total_ticks);
                    match end {
                        // No following @t and no declared duration leaves the count
                        // underivable. Emit none and let the fail-closed check below speak.
                        None => 0,
                        Some(end) if s.d > 0 => end.saturating_sub(cursor).div_ceil(s.d),
                        Some(_) => 0,
                    }
                }
                Some(r) => r as u64 + 1,
                None => 1,
            };
            for _ in 0..repeat {
                let number = segment_number(start_number, offset)?;
                if end_number.is_some_and(|end| number > end) {
                    break;
                }
                push_segment(&mut segment_urls, &media_tpl, number)?;
                offset += 1;
                cursor = cursor.saturating_add(s.d);
            }
        }
    } else if let Some(duration) = seg_duration {
        let seg_dur_secs = duration / timescale;
        if let Some(total) = total_secs
            && seg_dur_secs > 0.0
        {
            let count = (total / seg_dur_secs).ceil() as u64;
            for i in 0..count {
                let number = segment_number(start_number, i)?;
                if end_number.is_some_and(|end| number > end) {
                    break;
                }
                push_segment(&mut segment_urls, &media_tpl, number)?;
            }
        }
    }

    if segment_urls.is_empty() {
        anyhow::bail!(
            "DASH manifest yielded no segments: neither the segment timeline nor a declared \
             duration resolved to a repeat count"
        );
    }

    crate::vprintln!(
        "[DASH]   init_url={}",
        crate::util::truncate_str(&init_url, 80)
    );
    crate::vprintln!(
        "[DASH]   {} segment URLs, codec={}, sampleRate={:?}",
        segment_urls.len(),
        codec,
        sample_rate
    );

    Ok(DashManifest {
        init_url,
        segment_urls,
        codec,
        sample_rate,
        bandwidth,
        duration_secs: total_secs,
    })
}

#[cfg(test)]
#[path = "../../tests/unit/player/dash.rs"]
mod tests;
