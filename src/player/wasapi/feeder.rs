use std::io::{Read, Seek};
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{Arc, mpsc};

use symphonia::core::audio::GenericAudioBufferRef;
use symphonia::core::codecs::audio::AudioCodecParameters;
use symphonia::core::io::MediaSource;

use super::{ExclusiveCommand, StreamStart};
use crate::player::decode_feed::{self, Closed, Feed, FeedSink};

struct SizedMediaSource {
    inner: crate::player::buffer::RamBuffer,
    byte_len: u64,
}

impl Read for SizedMediaSource {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.inner.read(buf)
    }
}

impl Seek for SizedMediaSource {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        self.inner.seek(pos)
    }
}

impl MediaSource for SizedMediaSource {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        Some(self.byte_len)
    }
}

fn append_interleaved_i32_as_pcm(samples: &[i32], bits_per_sample: u32, out: &mut Vec<u8>) {
    if bits_per_sample <= 16 {
        // i32 samples -> take upper 16 bits (little-endian: bytes [2..4]).
        out.reserve(samples.len() * 2);
        for &s in samples {
            let b = s.to_le_bytes();
            out.push(b[2]);
            out.push(b[3]);
        }
    } else if bits_per_sample <= 24 {
        // i32 samples -> take upper 24 bits (bytes [1..4]).
        out.reserve(samples.len() * 3);
        for &s in samples {
            let b = s.to_le_bytes();
            out.push(b[1]);
            out.push(b[2]);
            out.push(b[3]);
        }
    } else {
        // 32-bit: pass through all 4 bytes.
        out.reserve(samples.len() * 4);
        for &s in samples {
            out.extend_from_slice(&s.to_le_bytes());
        }
    }
}

/// The exclusive side of the shared decode loop: the endpoint opens in the source's integer
/// depth, so each packet is packed to 16, 24 or 32-bit bytes and `consumed` counts bytes.
struct WasapiFeed {
    cmd_tx: mpsc::Sender<ExclusiveCommand>,
    stream_id: u32,
    opened_under: u32,
    start_paused: bool,
    consumed: Arc<AtomicU64>,
    // The source's depth and the endpoint's, both set by `adopt_format` before any packet.
    bits_per_sample: u32,
    stored_bps: u32,
    sample_buf: Vec<i32>,
    diag_logged: bool,
}

impl FeedSink for WasapiFeed {
    const TAG: &'static str = "[WASAPI]";

    fn stream_id(&self) -> u32 {
        self.stream_id
    }

    fn adopt_format(&mut self, params: &AudioCodecParameters) -> Result<(), String> {
        let bits_per_sample = params
            .bits_per_sample
            .ok_or_else(|| "no bits_per_sample".to_string())?;
        self.bits_per_sample = bits_per_sample;
        self.stored_bps = if bits_per_sample <= 16 {
            16
        } else if bits_per_sample <= 24 {
            24
        } else {
            32
        };
        Ok(())
    }

    fn units_per_sample(&self) -> u64 {
        self.stored_bps as u64 / 8
    }

    fn send(&self, feed: Feed) -> Result<(), Closed> {
        let stream_id = self.stream_id;
        let command = match feed {
            Feed::StartupFailed { error, cause } => ExclusiveCommand::StartupFailed {
                stream_id,
                error,
                cause,
            },
            Feed::StartStream {
                sample_rate,
                channels,
                duration_secs,
                start_secs,
            } => {
                crate::vprintln!(
                    "[WASAPI-DBG] flac reader: probe OK ({sample_rate}Hz {channels}ch {}bit), sending StartStream",
                    self.stored_bps
                );
                ExclusiveCommand::StartStream(StreamStart {
                    stream_id,
                    opened_under: self.opened_under,
                    sample_rate,
                    channels,
                    bits_per_sample: self.stored_bps,
                    duration_secs,
                    start_secs,
                    start_paused: self.start_paused,
                    consumed: self.consumed.clone(),
                })
            }
            Feed::ResetForSeek { gen_id, start_secs } => ExclusiveCommand::ResetForSeek {
                stream_id,
                gen_id,
                start_secs,
            },
            Feed::SeekFailed { gen_id } => ExclusiveCommand::SeekFailed { stream_id, gen_id },
            Feed::DecodeFailed { error, cause } => ExclusiveCommand::DecodeFailed {
                stream_id,
                error,
                cause,
            },
            Feed::EndStream => ExclusiveCommand::EndStream { stream_id },
        };
        self.cmd_tx.send(command).map_err(|_| Closed)
    }

    fn push(&mut self, decoded: GenericAudioBufferRef<'_>) -> Result<u64, Closed> {
        self.sample_buf.clear();
        decoded.copy_to_vec_interleaved::<i32>(&mut self.sample_buf);

        if !self.diag_logged && !self.sample_buf.is_empty() {
            self.diag_logged = true;
            let mn = self.sample_buf.iter().copied().min().unwrap_or(0);
            let mx = self.sample_buf.iter().copied().max().unwrap_or(0);
            crate::vprintln3!(
                "[WASAPI-DIAG] decode src_bps={} i32 min={mn} max={mx} raw=[{:#010x} {:#010x} {:#010x} {:#010x}]",
                self.bits_per_sample,
                self.sample_buf.first().copied().unwrap_or(0),
                self.sample_buf.get(1).copied().unwrap_or(0),
                self.sample_buf.get(2).copied().unwrap_or(0),
                self.sample_buf.get(3).copied().unwrap_or(0),
            );
        }

        let mut chunk = Vec::new();
        append_interleaved_i32_as_pcm(&self.sample_buf, self.bits_per_sample, &mut chunk);

        if !chunk.is_empty() {
            self.cmd_tx
                .send(ExclusiveCommand::PushPcm {
                    stream_id: self.stream_id,
                    pcm_data: chunk,
                })
                .map_err(|_| Closed)?;
        }
        Ok(self.sample_buf.len() as u64)
    }
}

#[expect(clippy::too_many_arguments)]
pub(in crate::player) fn stream_flac_reader_to_wasapi(
    reader: crate::player::buffer::RamBuffer,
    byte_len: u64,
    stream_id: u32,
    opened_under: u32,
    cmd_tx: mpsc::Sender<ExclusiveCommand>,
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
    let source = Box::new(SizedMediaSource {
        inner: reader,
        byte_len,
    });

    crate::vprintln!("[WASAPI-DBG] flac reader: starting probe (byte_len={byte_len})");
    let sink = WasapiFeed {
        cmd_tx,
        stream_id,
        opened_under,
        start_paused,
        consumed: consumed.clone(),
        bits_per_sample: 0,
        stored_bps: 0,
        sample_buf: Vec::new(),
        diag_logged: false,
    };
    decode_feed::run(
        sink,
        source,
        &whole,
        &cancel,
        seek_to,
        seek_gen_id,
        &seek_rx,
        &consumed,
    )
}
