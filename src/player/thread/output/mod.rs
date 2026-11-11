mod mixer;
mod resample;
mod stream;

#[cfg(test)]
pub(super) use mixer::pack_xfade_done;
pub(super) use mixer::{CrossfadeLink, CrossfadeSlot, unpack_xfade_done};
pub(super) use resample::AudioPipeline;
#[cfg(target_os = "windows")]
pub(in crate::player) use stream::find_output_device;
pub(crate) use stream::is_default_selector;
pub(super) use stream::{
    ENGINE_RATE_IS_PINNED, enumerate_audio_devices, open_output_stream, output_device_name,
    resolve_device, resolved_device_name,
};

/// `cpal_stream_error` signal codes: the cpal error callback (audio thread) stores one,
/// `poll_playback` (player thread) reads it. `DEVICE_LOST` triggers a pipeline rebuild.
pub(super) const STREAM_ERR_NONE: u8 = 0;
pub(super) const STREAM_ERR_DEVICE_LOST: u8 = 1;
pub(super) const STREAM_ERR_UNKNOWN: u8 = 2;

pub(super) fn format_sample_rate(rate: u32) -> String {
    if rate.is_multiple_of(1000) {
        format!("{} kHz", rate / 1000)
    } else {
        format!("{:.1} kHz", rate as f64 / 1000.0)
    }
}

pub(super) fn format_duration_mmss(secs: f64) -> String {
    let total = secs as u32;
    format!("{}:{:02}", total / 60, total % 60)
}

pub(super) fn codec_name(codec: symphonia::core::codecs::audio::AudioCodecId) -> &'static str {
    use symphonia::core::codecs::audio::well_known::*;
    match codec {
        CODEC_ID_FLAC => "flac",
        CODEC_ID_AAC => "aac",
        CODEC_ID_MP3 => "mp3",
        CODEC_ID_VORBIS => "vorbis",
        CODEC_ID_OPUS => "opus",
        CODEC_ID_PCM_S16LE | CODEC_ID_PCM_S24LE | CODEC_ID_PCM_S32LE | CODEC_ID_PCM_F32LE => "pcm",
        CODEC_ID_ALAC => "alac",
        _ => "unknown",
    }
}

pub(super) struct ProbeInfo {
    pub sample_rate: u32,
    pub channels: u16,
    pub duration: f64,
    pub bit_depth: Option<u32>,
    pub codec: &'static str,
}

pub(super) fn probe_audio_format(
    buffer: &crate::player::buffer::RamBuffer,
) -> Result<ProbeInfo, String> {
    use symphonia::core::codecs::CodecParameters;
    use symphonia::core::formats::probe::Hint;
    use symphonia::core::io::MediaSourceStream;

    let reader = buffer.clone();
    let mss = MediaSourceStream::new(Box::new(reader), Default::default());
    let format = symphonia::default::get_probe()
        .probe(
            &Hint::new(),
            mss,
            symphonia::core::formats::FormatOptions::default(),
            symphonia::core::meta::MetadataOptions::default(),
        )
        .map_err(|e| format!("Probe failed: {e}"))?;

    let (track, params) = format
        .tracks()
        .iter()
        .find_map(|t| match &t.codec_params {
            Some(CodecParameters::Audio(p)) => Some((t, p)),
            _ => None,
        })
        .ok_or_else(|| "No audio track found".to_string())?;

    let sample_rate = params.sample_rate.unwrap_or(44100);
    Ok(ProbeInfo {
        sample_rate,
        channels: params
            .channels
            .as_ref()
            .map(|c| c.count() as u16)
            .unwrap_or(2),
        duration: crate::player::length::track_secs(track, sample_rate, buffer),
        bit_depth: params.bits_per_sample,
        codec: codec_name(params.codec),
    })
}

#[cfg(test)]
#[path = "../../../../tests/unit/player/thread/output.rs"]
mod tests;

#[cfg(test)]
#[path = "../../../../tests/unit/player/thread/output/fixture.rs"]
mod fixture;
