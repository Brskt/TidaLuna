use core::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::Arc;
use std::sync::mpsc;

use symphonia::core::audio::GenericAudioBufferRef;
use symphonia::core::codecs::audio::AudioCodecParameters;

use super::AsioCommand;
use crate::player::buffer::RamBuffer;
use crate::player::decode_feed::{self, Closed, Feed, FeedSink};

/// The ASIO side of the shared decode loop: the ring is i32 whatever the source's depth,
/// so `PushPcm` carries the decoder's `Vec<i32>` as is (rendered as src_bps=32) and
/// `consumed` counts samples.
struct AsioFeed {
    cmd_tx: mpsc::Sender<AsioCommand>,
    stream_id: u32,
    opened_under: u32,
    start_paused: bool,
    consumed: Arc<AtomicU64>,
}

impl FeedSink for AsioFeed {
    const TAG: &'static str = "[ASIO]";

    fn stream_id(&self) -> u32 {
        self.stream_id
    }

    fn adopt_format(&mut self, _params: &AudioCodecParameters) -> Result<(), String> {
        Ok(())
    }

    fn units_per_sample(&self) -> u64 {
        1
    }

    fn send(&self, feed: Feed) -> Result<(), Closed> {
        let stream_id = self.stream_id;
        let command = match feed {
            Feed::StartupFailed { error, cause } => AsioCommand::StartupFailed {
                stream_id,
                error,
                cause,
            },
            Feed::StartStream {
                sample_rate,
                channels,
                duration_secs,
                start_secs,
            } => AsioCommand::StartStream {
                stream_id,
                opened_under: self.opened_under,
                sample_rate,
                channels,
                duration_secs,
                start_secs,
                start_paused: self.start_paused,
                consumed: self.consumed.clone(),
            },
            Feed::ResetForSeek { gen_id, start_secs } => AsioCommand::ResetForSeek {
                stream_id,
                gen_id,
                start_secs,
            },
            Feed::SeekFailed { gen_id } => AsioCommand::SeekFailed { stream_id, gen_id },
            Feed::DecodeFailed { error, cause } => AsioCommand::DecodeFailed {
                stream_id,
                error,
                cause,
            },
            Feed::EndStream => AsioCommand::EndStream { stream_id },
        };
        self.cmd_tx.send(command).map_err(|_| Closed)
    }

    fn push(&mut self, decoded: GenericAudioBufferRef<'_>) -> Result<u64, Closed> {
        let mut samples: Vec<i32> = Vec::new();
        decoded.copy_to_vec_interleaved::<i32>(&mut samples);

        let sample_count = samples.len() as u64;
        if !samples.is_empty() {
            self.cmd_tx
                .send(AsioCommand::PushPcm {
                    stream_id: self.stream_id,
                    samples,
                })
                .map_err(|_| Closed)?;
        }
        Ok(sample_count)
    }
}

/// Decode a `RamBuffer` (the same source the cpal/exclusive paths read) to interleaved i32
/// and feed the ASIO control thread via `AsioCommand`. `RamBuffer` is itself a
/// `MediaSource`, needing no `SizedMediaSource` wrapper.
#[expect(clippy::too_many_arguments)]
pub(crate) fn stream_reader_to_asio(
    reader: RamBuffer,
    stream_id: u32,
    opened_under: u32,
    cmd_tx: mpsc::Sender<AsioCommand>,
    cancel: Arc<AtomicBool>,
    seek_to: Option<f64>,
    // Identity of the initial seek, minted by the spawn alongside `stream_id`; the live
    // seeks that follow carry their own on the channel.
    seek_gen_id: u32,
    start_paused: bool,
    seek_rx: mpsc::Receiver<(f64, u32)>,
    consumed: Arc<AtomicU64>,
) -> Result<(), String> {
    let whole = reader.clone();
    let sink = AsioFeed {
        cmd_tx,
        stream_id,
        opened_under,
        start_paused,
        consumed: consumed.clone(),
    };
    decode_feed::run(
        sink,
        Box::new(reader),
        &whole,
        &cancel,
        seek_to,
        seek_gen_id,
        &seek_rx,
        &consumed,
    )
}
