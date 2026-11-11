//! Tests for `src/player/decode_feed.rs`, attached to it by `#[path]`: the shared loop driven
//! through a fake sink over a WAV source, no device involved.

use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use symphonia::core::audio::GenericAudioBufferRef;
use symphonia::core::codecs::audio::AudioCodecParameters;

use super::{Closed, Feed, FeedSink, run};
use crate::player::buffer::RamBuffer;
use crate::player::packet_failure::PacketFailure;
use crate::player::thread::wav_fixture::wav_s16_mono;

const RATE: u32 = 44_100;
const FRAMES: usize = 4_410;

/// What the loop sent, reduced to what these tests read.
#[derive(Debug, PartialEq)]
enum Seen {
    StartStream {
        sample_rate: u32,
        channels: u32,
        duration_secs: f64,
        start_secs: f64,
    },
    Pcm,
    ResetForSeek {
        gen_id: u32,
        start_secs: f64,
    },
    SeekFailed {
        gen_id: u32,
    },
    EndStream,
    Failed {
        error: String,
        cause: PacketFailure,
    },
}

struct FakeSink(mpsc::Sender<Seen>);

impl FeedSink for FakeSink {
    const TAG: &'static str = "[TEST]";

    fn stream_id(&self) -> u32 {
        1
    }

    fn adopt_format(&mut self, _params: &AudioCodecParameters) -> Result<(), String> {
        Ok(())
    }

    fn units_per_sample(&self) -> u64 {
        1
    }

    fn send(&self, feed: Feed) -> Result<(), Closed> {
        let seen = match feed {
            Feed::StartupFailed { error, cause } | Feed::DecodeFailed { error, cause } => {
                Seen::Failed { error, cause }
            }
            Feed::StartStream {
                sample_rate,
                channels,
                duration_secs,
                start_secs,
            } => Seen::StartStream {
                sample_rate,
                channels,
                duration_secs,
                start_secs,
            },
            Feed::ResetForSeek { gen_id, start_secs } => Seen::ResetForSeek { gen_id, start_secs },
            Feed::SeekFailed { gen_id } => Seen::SeekFailed { gen_id },
            Feed::EndStream => Seen::EndStream,
        };
        self.0.send(seen).map_err(|_| Closed)
    }

    fn push(&mut self, decoded: GenericAudioBufferRef<'_>) -> Result<u64, Closed> {
        let mut samples: Vec<i32> = Vec::new();
        decoded.copy_to_vec_interleaved::<i32>(&mut samples);
        if !samples.is_empty() {
            self.0.send(Seen::Pcm).map_err(|_| Closed)?;
        }
        Ok(samples.len() as u64)
    }
}

/// The loop on its own thread, as the engines spawn it, over a short complete WAV.
struct Feeding {
    seen: mpsc::Receiver<Seen>,
    seek_tx: mpsc::Sender<(f64, u32)>,
    thread: JoinHandle<Result<(), String>>,
}

impl Feeding {
    fn start() -> Self {
        let wav = wav_s16_mono(RATE, &[0; FRAMES]);
        let (seen_tx, seen) = mpsc::channel();
        let (seek_tx, seek_rx) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            let reader = RamBuffer::from_complete(wav);
            let whole = reader.clone();
            run(
                FakeSink(seen_tx),
                Box::new(reader),
                &whole,
                &AtomicBool::new(false),
                None,
                0,
                &seek_rx,
                &AtomicU64::new(0),
            )
        });
        Self {
            seen,
            seek_tx,
            thread,
        }
    }

    /// The next event other than PCM, with the count of PCM pushes before it.
    fn next(&self) -> (Seen, usize) {
        let mut pcm = 0;
        loop {
            match self.seen.recv_timeout(Duration::from_secs(10)) {
                Ok(Seen::Pcm) => pcm += 1,
                Ok(Seen::Failed { error, cause }) => panic!("the loop failed ({cause:?}): {error}"),
                Ok(seen) => return (seen, pcm),
                Err(e) => panic!("the loop went silent after {pcm} PCM pushes: {e}"),
            }
        }
    }

    /// The stream opens at 0 with the source's own format, whatever seek is to follow.
    fn assert_started(&self) {
        let (seen, pcm) = self.next();
        let Seen::StartStream {
            sample_rate,
            channels,
            duration_secs,
            start_secs,
        } = seen
        else {
            panic!("the loop opened with {seen:?}");
        };
        assert_eq!((sample_rate, channels, start_secs, pcm), (RATE, 1, 0.0, 0));
        let expected = FRAMES as f64 / RATE as f64;
        assert!(
            (duration_secs - expected).abs() < 1e-6,
            "duration {duration_secs}s, expected {expected}s"
        );
    }

    /// Hangs up the seek channel, the parked loop's only exit, and joins it.
    fn finish(self) {
        drop(self.seek_tx);
        let result = self.thread.join().expect("the loop panicked");
        assert_eq!(result, Ok(()), "a hung-up seek channel is no failure");
    }
}

/// The loop parks on the seek channel at EOF instead of returning: a cached source decodes far
/// ahead of playback, and a returned loop would leave the rest of the track unseekable.
#[test]
fn a_seek_after_eof_plays_the_stream_again() {
    let feeding = Feeding::start();
    feeding.assert_started();
    let (end, played) = feeding.next();
    assert_eq!(end, Seen::EndStream);
    assert!(played > 0, "the source decoded to nothing");

    feeding.seek_tx.send((0.0, 7)).unwrap();
    assert_eq!(
        feeding.next(),
        (
            Seen::ResetForSeek {
                gen_id: 7,
                start_secs: 0.0
            },
            0
        )
    );
    let (end, replayed) = feeding.next();
    assert_eq!(end, Seen::EndStream, "the seek did not re-end the stream");
    assert_eq!(replayed, played, "the seek did not decode the track again");
    feeding.finish();
}

/// A target `Time` cannot represent is refused with the seek's own id: dropped without a
/// reply, the player would wait on that seek forever.
#[test]
fn an_out_of_range_seek_is_refused_not_dropped() {
    let feeding = Feeding::start();
    feeding.assert_started();
    assert_eq!(feeding.next().0, Seen::EndStream);

    feeding.seek_tx.send((f64::MAX, 9)).unwrap();
    assert_eq!(feeding.next(), (Seen::SeekFailed { gen_id: 9 }, 0));
    assert_eq!(
        feeding.next(),
        (Seen::EndStream, 0),
        "the refusal must leave the loop parked"
    );
    feeding.finish();
}
