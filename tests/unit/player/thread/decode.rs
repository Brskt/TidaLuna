//! Tests for `src/player/thread/decode.rs`, attached to it by `#[path]`. `push_samples` takes
//! a ring producer and a channel, both of which a test can build: the contract that failed
//! once is checkable without a device or a media stream. `push_until_settled` takes its seek
//! as a closure for the same reason, which is what lets these tests state what a refused seek
//! owes without a container to demux.

use super::{
    DecodeCommand, DecodeEvent, DecodeThreadConfig, PushInterrupt, PushOutcome, PushRun,
    SeekOutcome, push_samples, push_until_settled, spawn_decode_thread,
};
use crate::player::buffer::RamBuffer;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::mpsc;
use std::time::{Duration, Instant};

fn ring(capacity: usize) -> (rtrb::Producer<f32>, rtrb::Consumer<f32>) {
    rtrb::RingBuffer::new(capacity)
}

#[test]
fn a_push_with_room_and_no_commands_drains() {
    let (mut producer, consumer) = ring(64);
    let (_tx, rx) = mpsc::channel();
    let counted = AtomicU64::new(0);
    let mut logged = true;

    assert!(matches!(
        push_samples(&[0.5f32; 8], &mut producer, &rx, &counted, &mut logged),
        PushOutcome::Drained
    ));
    assert_eq!(counted.load(Relaxed), 8, "the throttle was not credited");
    assert_eq!(consumer.slots(), 8, "the samples never reached the ring");
}

#[test]
fn a_queued_pause_is_handed_back_to_the_caller() {
    let (mut producer, _consumer) = ring(64);
    let (tx, rx) = mpsc::channel();
    tx.send(DecodeCommand::Pause).unwrap();
    let counted = AtomicU64::new(0);
    let mut logged = true;

    // The push takes the command off the channel; returning it is the only way the caller
    // learns the decoder should stop. Folding it in with a clean drain once left the loop
    // decoding under a paused user. The 0: a queued command precedes the first slot read.
    assert!(matches!(
        push_samples(&[0.5f32; 8], &mut producer, &rx, &counted, &mut logged),
        PushOutcome::Interrupted(PushInterrupt::Pause, 0)
    ));
}

#[test]
fn an_interruption_reports_how_far_the_push_got() {
    // The EOF flush cannot be replayed (the pipeline empties its accumulator as it
    // flushes), and its caller can only carry the remainder if the push says where it
    // stopped. Reporting a bare interruption once cost a resampled track its final
    // samples whenever a pause landed on them.
    let (mut producer, _consumer) = ring(4);
    let (tx, rx) = mpsc::channel();
    let counted = AtomicU64::new(0);
    let mut logged = true;

    // Nothing ever reads the ring: the push fills it and can go no further whatever the
    // pause's timing, making the count 4 by construction rather than by luck.
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(20));
        let _ = tx.send(DecodeCommand::Pause);
    });

    match push_samples(&[0.5f32; 8], &mut producer, &rx, &counted, &mut logged) {
        PushOutcome::Interrupted(PushInterrupt::Pause, written) => {
            assert_eq!(written, 4, "the caller cannot tell what is left to deliver");
        }
        _ => panic!("the pause did not reach the caller"),
    }
}

#[test]
fn a_queued_stop_is_handed_back_to_the_caller() {
    let (mut producer, _consumer) = ring(64);
    let (tx, rx) = mpsc::channel();
    tx.send(DecodeCommand::Stop).unwrap();
    let counted = AtomicU64::new(0);
    let mut logged = true;

    assert!(matches!(
        push_samples(&[0.5f32; 8], &mut producer, &rx, &counted, &mut logged),
        PushOutcome::Interrupted(PushInterrupt::Stop, _)
    ));
}

#[test]
fn a_queued_seek_carries_its_target_and_generation_back() {
    let (mut producer, _consumer) = ring(64);
    let (tx, rx) = mpsc::channel();
    tx.send(DecodeCommand::Seek(12.5, 7)).unwrap();
    let counted = AtomicU64::new(0);
    let mut logged = true;

    let outcome = push_samples(&[0.5f32; 8], &mut producer, &rx, &counted, &mut logged);
    match outcome {
        PushOutcome::Interrupted(PushInterrupt::Seek(time, gen_id), _) => {
            assert_eq!(time, 12.5);
            assert_eq!(
                gen_id, 7,
                "the generation an ack is matched on was rewritten"
            );
        }
        _ => panic!("the seek did not reach the caller"),
    }
}

#[test]
fn a_resume_does_not_interrupt_a_push() {
    let (mut producer, consumer) = ring(64);
    let (tx, rx) = mpsc::channel();
    tx.send(DecodeCommand::Resume).unwrap();
    let counted = AtomicU64::new(0);
    let mut logged = true;

    // Samples only flow while the decoder runs; a resume arriving mid-push asks for
    // nothing, and absorbing it keeps the caller from a decision with no content.
    assert!(matches!(
        push_samples(&[0.5f32; 8], &mut producer, &rx, &counted, &mut logged),
        PushOutcome::Drained
    ));
    assert_eq!(consumer.slots(), 8, "a resume cost the push its samples");
}

#[test]
fn a_pause_hands_back_only_what_the_ring_did_not_take() {
    // The per-packet push had nowhere to put this remainder: a pause landing mid-packet
    // dropped it while the reader had already moved past the packet. An ordinary pause left
    // the track short by up to one packet of audio.
    let (mut producer, consumer) = ring(4);
    let (tx, rx) = mpsc::channel();
    let counted = AtomicU64::new(0);
    let mut logged = true;
    let samples: Vec<f32> = vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0];

    // Nothing reads the ring, so the push stops at slot 4 whatever the pause's timing.
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(20));
        let _ = tx.send(DecodeCommand::Pause);
    });

    match push_until_settled(
        &samples,
        &mut producer,
        &rx,
        &counted,
        &mut logged,
        |_, _| SeekOutcome::Moved,
    ) {
        PushRun::Paused(rest) => assert_eq!(
            rest,
            vec![5.0, 6.0, 7.0, 8.0],
            "the samples the ring never took were not handed back"
        ),
        _ => panic!("the pause did not reach the caller"),
    }
    assert_eq!(
        consumer.slots(),
        4,
        "the samples that did land were replayed or lost"
    );
}

#[test]
fn a_refused_seek_pushes_the_rest_through() {
    let (mut producer, consumer) = ring(64);
    let (tx, rx) = mpsc::channel();
    tx.send(DecodeCommand::Seek(3.0, 1)).unwrap();
    let counted = AtomicU64::new(0);
    let mut logged = true;
    let mut seeks = 0;

    // A refusal leaves the reader on these samples, so they are still the ones owed: the run
    // carries on instead of reporting an interruption its caller would drop them on.
    let run = push_until_settled(
        &[0.5f32; 8],
        &mut producer,
        &rx,
        &counted,
        &mut logged,
        |_, _| {
            seeks += 1;
            SeekOutcome::Refused
        },
    );

    assert!(matches!(run, PushRun::Drained));
    assert_eq!(seeks, 1, "the seek was never attempted");
    assert_eq!(
        consumer.slots(),
        8,
        "the refused seek cost the buffer its samples"
    );
}

#[test]
fn a_refused_seek_resumes_where_the_push_stopped() {
    // Advancing by the reported count is what keeps the retry from re-sending what already
    // reached the ring: a track that repeats four samples is as wrong as one that drops them.
    let (mut producer, mut consumer) = ring(4);
    let (tx, rx) = mpsc::channel();
    let counted = AtomicU64::new(0);
    let mut logged = true;
    let samples: Vec<f32> = vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0];
    let mut received: Vec<f32> = Vec::new();

    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(20));
        let _ = tx.send(DecodeCommand::Seek(3.0, 1));
    });

    // The ring is full when the seek lands, so the refusal is paired with a reader that
    // empties it, which is what the output is doing while a seek is refused.
    let run = push_until_settled(
        &samples,
        &mut producer,
        &rx,
        &counted,
        &mut logged,
        |_, _| {
            while let Ok(s) = consumer.pop() {
                received.push(s);
            }
            SeekOutcome::Refused
        },
    );

    while let Ok(s) = consumer.pop() {
        received.push(s);
    }
    assert!(matches!(run, PushRun::Drained));
    assert_eq!(
        received, samples,
        "the retry replayed or skipped part of the buffer"
    );
}

#[test]
fn a_moved_seek_abandons_what_is_left() {
    let (mut producer, consumer) = ring(4);
    let (tx, rx) = mpsc::channel();
    let counted = AtomicU64::new(0);
    let mut logged = true;
    let samples: Vec<f32> = vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0];

    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(20));
        let _ = tx.send(DecodeCommand::Seek(3.0, 1));
    });

    // Past a seek that landed, these samples are the position the reader left; pushing them
    // on would splice the old position into the new one.
    let run = push_until_settled(
        &samples,
        &mut producer,
        &rx,
        &counted,
        &mut logged,
        |_, _| SeekOutcome::Moved,
    );

    assert!(matches!(run, PushRun::Moved));
    assert_eq!(consumer.slots(), 4, "the run kept pushing past the seek");
}

#[test]
fn a_stop_ends_the_run() {
    let (mut producer, _consumer) = ring(64);
    let (tx, rx) = mpsc::channel();
    tx.send(DecodeCommand::Stop).unwrap();
    let counted = AtomicU64::new(0);
    let mut logged = true;

    assert!(matches!(
        push_until_settled(
            &[0.5f32; 8],
            &mut producer,
            &rx,
            &counted,
            &mut logged,
            |_, _| SeekOutcome::Moved
        ),
        PushRun::Stopped
    ));
}

// --- Driving the real decode_loop ---
//
// The loop opens with a symphonia probe, so exercising it needs bytes symphonia will genuinely
// demux and decode. RIFF/WAVE carrying PCM S16LE is the cheapest such container: a 44-byte
// header, no checksum anywhere along the path, and `PcmDecoder` reads S16LE through untouched,
// so the values a fixture writes are the values that reach the ring. It is available because
// `Cargo.toml` leaves symphonia's default features on, and those carry `wav` and `pcm`; were
// that to change, these tests fail at the probe rather than quietly testing nothing.

/// Long enough that a stuck decode thread fails the test instead of hanging the suite.
const DEADLINE: Duration = Duration::from_secs(5);

fn wav_s16_mono(sample_rate: u32, samples: &[i16]) -> Vec<u8> {
    let data_len = samples.len() as u32 * 2;
    let mut wav = Vec::with_capacity(44 + data_len as usize);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVE");
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&(sample_rate * 2).to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    for sample in samples {
        wav.extend_from_slice(&sample.to_le_bytes());
    }
    wav
}

/// Ascending values rather than a constant tone: a gap, a repeat or a reordering in the ring is
/// visible in the drained sequence, which a tone would hide.
fn ramp(frames: usize) -> Vec<i16> {
    (1..=frames as i16).collect()
}

/// What the ramp becomes once the decoder normalises S16 against its full scale.
fn as_ring_samples(values: &[i16]) -> Vec<f32> {
    values.iter().map(|v| f32::from(*v) / 32768.0).collect()
}

struct Decoding {
    cmd_tx: mpsc::Sender<DecodeCommand>,
    events: mpsc::Receiver<DecodeEvent>,
    ring: rtrb::Consumer<f32>,
    decoded: Arc<AtomicU64>,
    completions: usize,
    /// Every seek answer in arrival order, generation and refusal together: a count alone
    /// cannot say whether the answer belonged to the dispatch that is still waiting.
    acks: Vec<(u32, bool)>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Decoding {
    fn start(bytes: Vec<u8>, ring_capacity: usize, output_rate: u32) -> Self {
        let (producer, ring) = rtrb::RingBuffer::new(ring_capacity);
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (event_tx, events) = mpsc::channel();
        let decoded = Arc::new(AtomicU64::new(0));
        let thread = spawn_decode_thread(DecodeThreadConfig {
            buffer: RamBuffer::from_complete(bytes),
            producer,
            decoded_samples: Arc::clone(&decoded),
            cmd_rx,
            event_tx,
            output_rate,
            output_channels: 1,
            seek_gen: Arc::new(AtomicU32::new(0)),
            reader_cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        })
        .expect("the OS gives this test its decode thread");
        Self {
            cmd_tx,
            events,
            ring,
            decoded,
            completions: 0,
            acks: Vec::new(),
            thread: Some(thread),
        }
    }

    fn send(&self, cmd: DecodeCommand) {
        self.cmd_tx.send(cmd).expect("the decode thread is gone");
    }

    /// Waits on the counter the decode thread shares with the ring, never on the clock: probe
    /// and decoder init take a variable time, so a sleep would guess where a command lands.
    fn wait_until_decoded(&self, target: u64) {
        let started = Instant::now();
        while self.decoded.load(Relaxed) < target {
            assert!(
                started.elapsed() < DEADLINE,
                "the ring never took {target} samples"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn drain(&mut self, into: &mut Vec<f32>) {
        while let Ok(sample) = self.ring.pop() {
            into.push(sample);
        }
    }

    fn take_events(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            match event {
                DecodeEvent::Finished => self.completions += 1,
                DecodeEvent::SeekComplete {
                    gen_id, refused, ..
                } => self.acks.push((gen_id, refused)),
                DecodeEvent::Error(e) => panic!("the decode thread reported: {e}"),
            }
        }
    }

    fn refusals(&self) -> usize {
        self.acks.iter().filter(|(_, refused)| *refused).count()
    }

    /// Drains until the track is announced done. Completion is the only signal that says no
    /// sample is still owed, so a test that stopped at a sample count would pass on a truncated
    /// track.
    fn drain_until_complete(&mut self, into: &mut Vec<f32>) {
        let started = Instant::now();
        while self.completions == 0 {
            self.drain(into);
            self.take_events();
            assert!(
                started.elapsed() < DEADLINE,
                "the track never announced completion ({} samples drained)",
                into.len()
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        self.drain(into);
    }

    fn wait_for_refusal(&mut self) {
        let started = Instant::now();
        while self.refusals() == 0 {
            self.take_events();
            assert!(
                started.elapsed() < DEADLINE,
                "the seek was never answered as refused"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Stops the thread, joins it, then takes whatever it emitted on the way out. A joined
    /// thread cannot announce anything more, so counts read after this are final: that is what
    /// lets a test state an absence instead of waiting for one and hoping the wait was long
    /// enough, which on a loaded machine it would not be.
    fn stop(&mut self) {
        let _ = self.cmd_tx.send(DecodeCommand::Stop);
        if let Some(thread) = self.thread.take() {
            thread.join().expect("the decode thread panicked");
        }
        self.take_events();
    }
}

#[test]
fn a_pause_mid_packet_delivers_every_sample_across_the_resume() {
    // The defect: the per-packet push dropped whatever the ring had not taken when a pause
    // landed, and the reader had already moved past that packet, so the samples were gone for
    // the track. Rates match here, so nothing stands between the container and the ring.
    const RATE: u32 = 44_100;
    const FRAMES: usize = 2_500;
    const RING: usize = 512;

    let mut decoding = Decoding::start(wav_s16_mono(RATE, &ramp(FRAMES)), RING, RATE);
    let mut received: Vec<f32> = Vec::new();

    decoding.send(DecodeCommand::Resume);
    // A packet holds 1152 frames and nothing drains the ring, so the first push fills it and
    // can go no further. The pause is therefore taken by the push, at that offset, rather than
    // by the command loop: `push_samples` reads the channel before it looks at free slots.
    decoding.wait_until_decoded(RING as u64);
    decoding.send(DecodeCommand::Pause);
    decoding.send(DecodeCommand::Resume);

    decoding.drain_until_complete(&mut received);
    decoding.stop();

    assert_eq!(
        received,
        as_ring_samples(&ramp(FRAMES)),
        "the pause cost the track samples, or delivered them out of order"
    );
}

#[test]
fn a_seek_taken_during_a_pause_does_not_splice_the_carried_tail_back_in() {
    // The other half of carrying a tail: once the reader leaves the position those samples came
    // from, delivering them would splice the old position into the new one.
    const RATE: u32 = 44_100;
    const FRAMES: usize = 2_500;
    const RING: usize = 512;
    // WAV packetises in blocks of 1152 frames and snaps a seek down onto that grid, so a target
    // inside the second block lands exactly on frame 1152.
    let inside_second_block = 1_500.0 / f64::from(RATE);

    let mut decoding = Decoding::start(wav_s16_mono(RATE, &ramp(FRAMES)), RING, RATE);
    let mut received: Vec<f32> = Vec::new();

    decoding.send(DecodeCommand::Resume);
    decoding.wait_until_decoded(RING as u64);
    decoding.send(DecodeCommand::Pause);
    decoding.send(DecodeCommand::Seek(inside_second_block, 1));
    decoding.send(DecodeCommand::Resume);

    decoding.drain_until_complete(&mut received);
    decoding.stop();

    // What the ring already held stays: only the tail still in hand is the seek's to discard.
    let mut expected = ramp(FRAMES);
    expected.drain(RING..1_152);
    assert_eq!(
        received,
        as_ring_samples(&expected),
        "a fragment from before the seek was delivered after it"
    );
}

#[test]
fn a_refused_seek_while_resuming_a_carried_tail_still_delivers_it() {
    // Under `CHUNK_SIZE` frames the pipeline emits nothing per packet and hands the whole track
    // back from its end-of-stream flush, which is the only push that can fill `pending_tail`.
    // That makes the parked resume reachable without timing anything.
    const SOURCE_RATE: u32 = 44_100;
    const OUTPUT_RATE: u32 = 48_000;
    const FRAMES: usize = 800;
    let fixture = wav_s16_mono(SOURCE_RATE, &ramp(FRAMES));

    // The resampler makes the output values its own, so the reference is the same fixture run
    // through the same rates without interruption.
    let mut baseline: Vec<f32> = Vec::new();
    let mut uninterrupted = Decoding::start(fixture.clone(), 8_192, OUTPUT_RATE);
    uninterrupted.send(DecodeCommand::Resume);
    uninterrupted.drain_until_complete(&mut baseline);
    uninterrupted.stop();
    assert!(
        baseline.len() > 2,
        "the flush produced no audio to carry: {} samples",
        baseline.len()
    );

    let ring = baseline.len() / 2;
    let mut decoding = Decoding::start(fixture, ring, OUTPUT_RATE);
    let mut received: Vec<f32> = Vec::new();

    decoding.send(DecodeCommand::Resume);
    // Half the flush fits, so the push stalls and the pause lands on it: the remainder becomes
    // the tail the park loop holds.
    decoding.wait_until_decoded(ring as u64);
    decoding.send(DecodeCommand::Pause);
    // Queued in this order, the resume starts the tail's push and its first channel read takes
    // the seek, so the refusal is answered at offset 0 of the tail rather than at a guessed
    // moment. Past the end of an 800-frame track, the seek cannot land.
    decoding.send(DecodeCommand::Resume);
    decoding.send(DecodeCommand::Seek(60.0, 2));
    decoding.wait_for_refusal();

    decoding.drain_until_complete(&mut received);

    assert_eq!(
        received, baseline,
        "the refused seek cost the track the tail it was still owed"
    );
    assert_eq!(
        decoding.completions, 1,
        "completion was announced more than once"
    );
    decoding.stop();
}

#[test]
fn a_refused_seek_while_parked_does_not_announce_a_second_completion() {
    // A refusal leaves the reader where it was, at end of stream. Leaving the park loop sent it
    // back through the end-of-stream branch, which announced the track finished a second time.
    const RATE: u32 = 44_100;
    const FRAMES: usize = 800;

    let mut decoding = Decoding::start(wav_s16_mono(RATE, &ramp(FRAMES)), 8_192, RATE);
    let mut received: Vec<f32> = Vec::new();

    decoding.send(DecodeCommand::Resume);
    decoding.drain_until_complete(&mut received);
    assert_eq!(received.len(), FRAMES, "the track did not arrive whole");

    decoding.send(DecodeCommand::Seek(60.0, 3));
    decoding.wait_for_refusal();
    // Stop and join before counting: an exited thread cannot announce anything more, which is
    // what makes the absence provable. Waiting a bounded while instead would pass on a loaded
    // machine whatever the code did, and stop asserting anything at all.
    decoding.stop();

    assert_eq!(
        decoding.completions, 1,
        "the refused seek announced the track finished again"
    );
}

#[test]
fn every_dispatched_seek_is_answered_once_under_its_own_generation() {
    // Not a defect this pins but a contract: six places in the loop take a `Seek` off the
    // channel, and an answer that never arrives leaves the player's seek flag set for good.
    // Two of them reach `do_decode_seek` by different routes, so both are driven here: one
    // through the interrupt closure of a push, one through the command loop's direct call.
    const RATE: u32 = 44_100;
    const FRAMES: usize = 2_500;
    const RING: usize = 512;
    let second_block = 1_152.0 / f64::from(RATE);
    let third_block = 2_304.0 / f64::from(RATE);

    let mut decoding = Decoding::start(wav_s16_mono(RATE, &ramp(FRAMES)), RING, RATE);
    let mut received: Vec<f32> = Vec::new();

    decoding.send(DecodeCommand::Resume);
    // Stalled on a full ring, the push is what reads the channel, so this seek goes through its
    // closure. The pause behind it then parks the loop on its blocking read, which is what
    // leaves the next seek to the command loop instead.
    decoding.wait_until_decoded(RING as u64);
    decoding.send(DecodeCommand::Seek(second_block, 11));
    decoding.send(DecodeCommand::Pause);
    decoding.send(DecodeCommand::Seek(third_block, 22));
    decoding.send(DecodeCommand::Resume);

    decoding.drain_until_complete(&mut received);

    assert_eq!(
        decoding.acks,
        vec![(11, false), (22, false)],
        "a dispatched seek is owed exactly one answer, carrying its own generation"
    );
    decoding.stop();
}
