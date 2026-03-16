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
}

/// Parse a DASH MPD XML string and extract segment URLs.
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
    let sample_rate = repr
        .audioSamplingRate
        .as_deref()
        .and_then(|s| s.parse::<u32>().ok());
    let bandwidth = repr.bandwidth.map(|b| b as u32);

    let seg_tpl = repr
        .SegmentTemplate
        .as_ref()
        .or(adaptation.SegmentTemplate.as_ref())
        .ok_or_else(|| anyhow::anyhow!("No SegmentTemplate found in MPD"))?;

    let init_url = seg_tpl
        .initialization
        .clone()
        .ok_or_else(|| anyhow::anyhow!("SegmentTemplate has no initialization URL"))?;

    let media_tpl = seg_tpl
        .media
        .clone()
        .ok_or_else(|| anyhow::anyhow!("SegmentTemplate has no media URL template"))?;

    let start_number = seg_tpl.startNumber.unwrap_or(1);

    let mut segment_urls = Vec::new();
    if let Some(timeline) = &seg_tpl.SegmentTimeline {
        let mut number = start_number;
        for s in &timeline.segments {
            let repeat = s.r.unwrap_or(0).max(0) as u64 + 1;
            for _ in 0..repeat {
                segment_urls.push(media_tpl.replace("$Number$", &number.to_string()));
                number += 1;
            }
        }
    } else if let Some(duration) = seg_tpl.duration {
        let timescale = seg_tpl.timescale.unwrap_or(1) as f64;
        if let Some(mpd_dur) = mpd.mediaPresentationDuration.as_ref() {
            let total_secs = mpd_dur.as_secs_f64();
            let seg_dur_secs = duration / timescale;
            let count = (total_secs / seg_dur_secs).ceil() as u64;
            for i in 0..count {
                segment_urls.push(media_tpl.replace("$Number$", &(start_number + i).to_string()));
            }
        }
    }

    crate::vprintln!("[DASH]   init_url={}", &init_url[..init_url.len().min(80)]);
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
    })
}
