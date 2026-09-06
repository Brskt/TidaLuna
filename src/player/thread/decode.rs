use super::output::AudioPipeline;
use super::{DecodeCommand, DecodeEvent};
use crate::player::buffer::{RamBuffer, ReadStop};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::mpsc;
use std::time::Duration;
use symphonia::core::codecs::CodecParameters;
use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, SeekMode, SeekTo};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;

pub(super) struct DecodeThreadConfig {
    pub buffer: RamBuffer,
    pub producer: rtrb::Producer<f32>,
    pub decoded_samples: Arc<AtomicU64>,
    pub cmd_rx: mpsc::Receiver<DecodeCommand>,
    pub event_tx: mpsc::Sender<DecodeEvent>,
    pub output_rate: u32,
    pub output_channels: u16,
    pub seek_gen: Arc<AtomicU32>,
    /// Retires this thread's reader. A field rather than a call the spawner has to remember:
    /// a decoder parked in a starved read answers no command, and whoever joins it waits.
    pub reader_cancel: Arc<AtomicBool>,
}

/// A command that ends a push, taken off the channel and owed to the caller. `Resume` is
/// absent on purpose: it changes nothing while samples are still flowing.
enum PushInterrupt {
    Stop,
    Pause,
    Seek(f64, u32),
}

enum PushOutcome {
    /// Every sample reached the ring.
    Drained,
    /// The push stopped here, and this command is still unhandled. The count is how many
    /// samples reached the ring; a producer that cannot replay its output needs it to know
    /// what is left to deliver.
    Interrupted(PushInterrupt, usize),
}

/// The one path samples take to the ring. A push that cannot answer `Stop` leaves a decode
/// thread nothing can retire, and both callers need that handling.
fn push_samples(
    samples: &[f32],
    producer: &mut rtrb::Producer<f32>,
    cmd_rx: &mpsc::Receiver<DecodeCommand>,
    decoded_samples: &AtomicU64,
    first_push_logged: &mut bool,
) -> PushOutcome {
    let mut offset = 0;
    while offset < samples.len() {
        if let Ok(cmd) = cmd_rx.try_recv() {
            let interrupt = match cmd {
                DecodeCommand::Stop => Some(PushInterrupt::Stop),
                DecodeCommand::Pause => Some(PushInterrupt::Pause),
                DecodeCommand::Seek(time, gen_id) => Some(PushInterrupt::Seek(time, gen_id)),
                DecodeCommand::Resume => None,
            };
            if let Some(interrupt) = interrupt {
                return PushOutcome::Interrupted(interrupt, offset);
            }
        }

        let available = producer.slots();
        if available == 0 {
            std::thread::sleep(Duration::from_millis(1));
            continue;
        }

        let to_write = (samples.len() - offset).min(available);
        match producer.write_chunk_uninit(to_write) {
            Ok(chunk) => {
                offset += chunk.fill_from_iter(samples[offset..offset + to_write].iter().copied());
                decoded_samples.fetch_add(to_write as u64, Relaxed);
                if !*first_push_logged {
                    *first_push_logged = true;
                    crate::vprintln!("[DECODE] First push to ring buffer: {} samples", to_write);
                }
            }
            // Unreachable as called: the ring refuses only a count above the slots it re-reads
            // for itself, `to_write` never exceeds the count read a line above, and only the
            // consumer moves the head, always freeing more. Yielding rather than failing keeps
            // that from becoming fatal.
            Err(_) => std::thread::sleep(Duration::from_millis(1)),
        }
    }
    PushOutcome::Drained
}

/// `None` when the OS refuses the thread, for the caller to report like any other pipeline
/// failure. Panicking here would unwind the player thread instead, and nothing respawns it:
/// the process keeps running with a transport that answers no command.
pub(super) fn spawn_decode_thread(
    mut cfg: DecodeThreadConfig,
) -> Option<std::thread::JoinHandle<()>> {
    // Bound the reader to this thread here: no spawn site can hand the buffer over
    // without the signal that retires it.
    cfg.buffer = cfg
        .buffer
        .clone()
        .with_reader_cancel(cfg.reader_cancel.clone());
    match std::thread::Builder::new()
        .name("decode".into())
        .spawn(move || {
            decode_loop(cfg);
        }) {
        Ok(handle) => Some(handle),
        Err(e) => {
            crate::verr!("[DECODE] cannot spawn the decode thread: {e}");
            None
        }
    }
}

struct DecodeContext<'a> {
    track_id: u32,
    source_rate: u32,
    output_rate: u32,
    output_channels: u16,
    seek_gen: &'a AtomicU32,
    decoded_samples: &'a AtomicU64,
    event_tx: &'a mpsc::Sender<DecodeEvent>,
}

/// Where the decoder stands right now, from the counter it shares with the ring. Used for
/// the two outcomes that leave it untouched; the landing branch reports the timestamp the
/// reader actually returned, which is exact at the source rate rather than the output one.
fn undisturbed_position_secs(ctx: &DecodeContext) -> f64 {
    let frames = ctx.decoded_samples.load(Relaxed) / ctx.output_channels.max(1) as u64;
    frames as f64 / ctx.output_rate.max(1) as f64
}

/// Whether the seek moved the reader. A refusal leaves it exactly where it was, which is what
/// tells a caller still holding unpushed samples that they are wanted.
#[derive(PartialEq, Eq)]
enum SeekOutcome {
    Moved,
    Refused,
}

fn do_decode_seek(
    time: f64,
    gen_id: u32,
    format: &mut dyn symphonia::core::formats::FormatReader,
    decoder: &mut dyn AudioDecoder,
    pipeline: &mut Option<AudioPipeline>,
    ctx: &DecodeContext,
) -> SeekOutcome {
    let seek_start = std::time::Instant::now();
    let Some(time_pos) = symphonia::core::units::Time::try_from_secs_f64(time) else {
        crate::vprintln!("[SEEK]   invalid seek target: {time}");
        let _ = ctx.event_tx.send(DecodeEvent::SeekComplete {
            gen_id,
            position: undisturbed_position_secs(ctx),
            refused: true,
        });
        return SeekOutcome::Refused;
    };
    let seek_to = SeekTo::Time {
        time: time_pos,
        track_id: Some(ctx.track_id),
    };
    match format.seek(SeekMode::Coarse, seek_to) {
        Ok(seeked) => {
            let seek_dur = seek_start.elapsed();
            decoder.reset();
            if let Some(p) = pipeline {
                p.reset();
            }
            ctx.seek_gen.fetch_add(1, Relaxed);
            let actual_ts = seeked.actual_ts.get() as u64;
            let out_ts = actual_ts * ctx.output_rate as u64 / ctx.source_rate as u64;
            ctx.decoded_samples
                .store(out_ts * ctx.output_channels as u64, Relaxed);
            let _ = ctx.event_tx.send(DecodeEvent::SeekComplete {
                gen_id,
                position: actual_ts as f64 / ctx.source_rate.max(1) as f64,
                refused: false,
            });
            let seek_ms = seek_dur.as_secs_f64() * 1000.0;
            if seek_ms >= 1.0 {
                crate::vprintln!("[SEEK]   decode: {:.0}ms (ts: {})", seek_ms, actual_ts);
            } else {
                crate::vprintln!(
                    "[SEEK]   decode: {:.0}µs (ts: {})",
                    seek_dur.as_micros(),
                    actual_ts
                );
            }
            SeekOutcome::Moved
        }
        Err(e) => {
            let _ = ctx.event_tx.send(DecodeEvent::SeekComplete {
                gen_id,
                position: undisturbed_position_secs(ctx),
                refused: true,
            });
            crate::vprintln!("[SEEK]   symphonia seek failed: {e}");
            SeekOutcome::Refused
        }
    }
}

/// How a push of one buffer ended. `Paused` carries what the ring has not taken: neither the
/// pipeline nor the reader hands those samples back a second time; dropping them here
/// drops them for the track.
enum PushRun {
    Drained,
    Stopped,
    Paused(Vec<f32>),
    /// A seek moved the reader, which makes what is left the old position's audio.
    Moved,
}

/// Pushes a whole buffer and owns the retain-or-drop rule its callers kept diverging on: a
/// refused seek leaves the reader on these samples and the rest still goes through; a moved
/// one makes them the old position's; a pause owes them back. Seeking arrives as a closure so
/// the rule can be exercised without a container to demux.
fn push_until_settled(
    samples: &[f32],
    producer: &mut rtrb::Producer<f32>,
    cmd_rx: &mpsc::Receiver<DecodeCommand>,
    decoded_samples: &AtomicU64,
    first_push_logged: &mut bool,
    mut seek: impl FnMut(f64, u32) -> SeekOutcome,
) -> PushRun {
    let mut from = 0usize;
    loop {
        match push_samples(
            &samples[from..],
            producer,
            cmd_rx,
            decoded_samples,
            first_push_logged,
        ) {
            PushOutcome::Drained => return PushRun::Drained,
            PushOutcome::Interrupted(PushInterrupt::Stop, _) => return PushRun::Stopped,
            PushOutcome::Interrupted(PushInterrupt::Pause, written) => {
                return PushRun::Paused(samples[from + written..].to_vec());
            }
            PushOutcome::Interrupted(PushInterrupt::Seek(time, gen_id), written) => {
                if seek(time, gen_id) == SeekOutcome::Moved {
                    return PushRun::Moved;
                }
                from += written;
            }
        }
    }
}

/// The seek [`push_until_settled`] takes. Built fresh at each call site rather than held
/// across the loop: the code between those calls uses the same reader, decoder and pipeline
/// directly, and a value keeping all three borrowed would collide with every one of them.
fn seek_closure<'a, 'c>(
    format: &'a mut dyn symphonia::core::formats::FormatReader,
    decoder: &'a mut dyn AudioDecoder,
    pipeline: &'a mut Option<AudioPipeline>,
    ctx: &'a DecodeContext<'c>,
) -> impl FnMut(f64, u32) -> SeekOutcome + 'a {
    move |time, gen_id| do_decode_seek(time, gen_id, format, decoder, pipeline, ctx)
}

/// Which deliberate stop ended a read, if that is what ended it. Asked of the error's payload
/// rather than of its kind, because `ErrorKind::Other` is a shared vocabulary: the buffer keeps
/// every real failure out of it by naming discipline alone, and symphonia mints its own `Other`
/// inside the Vorbis bit reader. The payload is put there by the site that knew why.
fn requested_stop(e: &symphonia::core::errors::Error) -> Option<ReadStop> {
    match e {
        symphonia::core::errors::Error::IoError(io) => ReadStop::from_io(io),
        _ => None,
    }
}

fn decode_loop(cfg: DecodeThreadConfig) {
    let DecodeThreadConfig {
        buffer,
        mut producer,
        decoded_samples,
        cmd_rx,
        event_tx,
        output_rate,
        output_channels,
        seek_gen,
        // Bound rather than dropped, because at one site the loop does have to poll it. A read
        // that fails names its stop in the error's payload, which is the precise answer and the
        // one `next_packet` is handed, but symphonia's probe scans for a format marker with
        // `while let Ok(byte) = mss.read_byte()` and discards that error, reporting a missing
        // format reader instead. A stop during the probe is recognisable only from the state it
        // was posted to.
        reader_cancel,
    } = cfg;
    crate::vprintln!("[DECODE] Thread started, probing format...");
    // Cloned before the buffer is boxed into the stream, which consumes it. Cancelling the
    // whole buffer has no reader-side view otherwise, and the probe is where that view is the
    // only one left.
    let stop_state = buffer.clone();
    let mss = MediaSourceStream::new(Box::new(buffer), Default::default());

    let hint = Hint::new();
    let format_opts = FormatOptions::default();
    let metadata_opts = MetadataOptions::default();
    let decoder_opts = AudioDecoderOptions::default();

    let mut format =
        match symphonia::default::get_probe().probe(&hint, mss, format_opts, metadata_opts) {
            Ok(f) => f,
            Err(e) => {
                // A stop lands here too: a fade whose decoder dies before it started retires
                // exactly this reader, and announced as a probe failure it took the track's
                // cache entry down and raised a media error for a track that was readable.
                // Both channels are asked because the probe has two exits: a reader it already
                // chose re-raises the read's error, where the marker scan discards it and only
                // the state still knows.
                if requested_stop(&e).is_some()
                    || stop_state.is_cancelled()
                    || reader_cancel.load(Relaxed)
                {
                    crate::vprintln!("[DECODE] Stopped while probing");
                    let _ = event_tx.send(DecodeEvent::Stopped);
                    return;
                }
                let _ = event_tx.send(DecodeEvent::Error(format!("probe failed: {e}")));
                return;
            }
        };

    let track = match format
        .tracks()
        .iter()
        .find(|t| matches!(&t.codec_params, Some(CodecParameters::Audio(_))))
    {
        Some(t) => t.clone(),
        None => {
            let _ = event_tx.send(DecodeEvent::Error("no audio track found".into()));
            return;
        }
    };

    let track_id = track.id;
    let audio_params = match &track.codec_params {
        Some(CodecParameters::Audio(p)) => p,
        _ => unreachable!("track was selected as audio"),
    };
    let codec_id = audio_params.codec;
    let source_rate = audio_params.sample_rate.unwrap_or(44100);
    let source_channels = audio_params
        .channels
        .as_ref()
        .map(|c| c.count())
        .unwrap_or(2);

    let mut decoder =
        match symphonia::default::get_codecs().make_audio_decoder(audio_params, &decoder_opts) {
            Ok(d) => d,
            Err(e) => {
                let _ = event_tx.send(DecodeEvent::Error(format!("codec init failed: {e}")));
                return;
            }
        };

    let mut pipeline = if source_rate != output_rate || source_channels != output_channels as usize
    {
        match AudioPipeline::new(
            source_rate,
            output_rate,
            source_channels,
            output_channels as usize,
        ) {
            Ok(pipe) => {
                crate::vprintln!(
                    "[DECODE] {}: {}Hz/{}ch -> {}Hz/{}ch",
                    if pipe.resamples() {
                        "Resampling"
                    } else {
                        "Channel remap"
                    },
                    source_rate,
                    source_channels,
                    output_rate,
                    output_channels
                );
                Some(pipe)
            }
            Err(e) => {
                let _ = event_tx.send(DecodeEvent::Error(e));
                return;
            }
        }
    } else {
        None
    };

    crate::vprintln!(
        "[DECODE] Probe OK: {} {}Hz/{}ch | output: {}Hz/{}ch",
        super::output::codec_name(codec_id),
        source_rate,
        source_channels,
        output_rate,
        output_channels
    );

    let decode_ctx = DecodeContext {
        track_id,
        source_rate,
        output_rate,
        output_channels,
        seek_gen: &seek_gen,
        decoded_samples: &decoded_samples,
        event_tx: &event_tx,
    };

    let mut sample_vec: Vec<f32> = Vec::new();
    // What a pause cut off mid-packet. It outlives the iteration that decoded it because the
    // packet is gone from the reader by then: `next_packet` returns the one after it.
    let mut carried_tail: Vec<f32> = Vec::new();
    let mut paused = true;
    let mut first_packet_logged = false;
    let mut first_push_logged = false;

    loop {
        // Process commands - block when paused (zero CPU), poll when active.
        loop {
            let cmd = if paused {
                match cmd_rx.recv() {
                    Ok(cmd) => cmd,
                    Err(_) => return,
                }
            } else {
                match cmd_rx.try_recv() {
                    Ok(cmd) => cmd,
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => return,
                }
            };
            match cmd {
                DecodeCommand::Seek(time, gen_id) => {
                    // A carried tail is the old position's once the reader leaves it.
                    if do_decode_seek(
                        time,
                        gen_id,
                        &mut *format,
                        &mut *decoder,
                        &mut pipeline,
                        &decode_ctx,
                    ) == SeekOutcome::Moved
                    {
                        carried_tail.clear();
                    }
                }
                DecodeCommand::Pause => {
                    paused = true;
                }
                DecodeCommand::Resume => {
                    crate::vprintln!("[DECODE] Resumed");
                    paused = false;
                }
                DecodeCommand::Stop => {
                    crate::vprintln!("[DECODE] Stop received, exiting");
                    return;
                }
            }
        }

        // The ring drains again: a tail the pause cut off is delivered before anything new is
        // decoded (pushing it after the next packet would play the two out of order).
        if !carried_tail.is_empty() {
            match push_until_settled(
                &carried_tail,
                &mut producer,
                &cmd_rx,
                &decoded_samples,
                &mut first_push_logged,
                seek_closure(&mut *format, &mut *decoder, &mut pipeline, &decode_ctx),
            ) {
                PushRun::Drained => carried_tail.clear(),
                PushRun::Stopped => return,
                PushRun::Paused(rest) => {
                    carried_tail = rest;
                    paused = true;
                    continue;
                }
                PushRun::Moved => {
                    carried_tail.clear();
                    continue;
                }
            }
        }

        let packet = match format.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) => {
                // What the flush produced and the ring has not taken yet. The pipeline
                // empties its accumulator as it flushes, and a second one hands back
                // nothing: an interrupted tail has to be carried, not re-derived.
                let mut pending_tail: Vec<f32> = Vec::new();
                // End of stream - flush resampler pipeline before signaling completion
                if let Some(ref mut pipe) = pipeline {
                    let flushed = match pipe.flush() {
                        Ok(f) => f,
                        Err(e) => {
                            let _ = event_tx.send(DecodeEvent::Error(e));
                            let _ = event_tx.send(DecodeEvent::Finished);
                            return;
                        }
                    };
                    match push_until_settled(
                        &flushed,
                        &mut producer,
                        &cmd_rx,
                        &decoded_samples,
                        &mut first_push_logged,
                        seek_closure(&mut *format, &mut *decoder, &mut pipeline, &decode_ctx),
                    ) {
                        PushRun::Drained => {}
                        PushRun::Stopped => {
                            crate::vprintln!("[DECODE] Stop received during flush, exiting");
                            return;
                        }
                        // A pause does not make these samples uninteresting, it only stops the
                        // ring from draining. Pushing through would spin on a ring nothing
                        // empties; the resume delivers them instead.
                        PushRun::Paused(rest) => {
                            pending_tail = rest;
                            paused = true;
                        }
                        PushRun::Moved => continue,
                    }
                }
                // Only claim the track is delivered once it is. A tail still owed is
                // announced by the resume that finally lets it through.
                if pending_tail.is_empty() {
                    let _ = event_tx.send(DecodeEvent::Finished);
                }
                crate::vprintln!("[DECODE] EOF, parked for seeks");
                // Park rather than return: the reader can still move back before EOF.
                // Returning drops cmd_rx, and a later seek then reaches a dead channel that
                // answers no SeekComplete. A Stop or a dropped sender is the only way out.
                loop {
                    match cmd_rx.recv() {
                        Ok(DecodeCommand::Seek(time, gen_id)) => {
                            // A refusal leaves the reader at EOF with the tail still owed, so
                            // this stays parked to keep it. Pushing it here is not the
                            // alternative: arriving paused, it would spin on a ring the pause
                            // stopped draining.
                            if do_decode_seek(
                                time,
                                gen_id,
                                &mut *format,
                                &mut *decoder,
                                &mut pipeline,
                                &decode_ctx,
                            ) == SeekOutcome::Moved
                            {
                                break;
                            }
                        }
                        Ok(DecodeCommand::Pause) => paused = true,
                        Ok(DecodeCommand::Resume) => {
                            paused = false;
                            if pending_tail.is_empty() {
                                continue;
                            }
                            // The output drains the ring again: what the pause cut off can
                            // finish now.
                            match push_until_settled(
                                &pending_tail,
                                &mut producer,
                                &cmd_rx,
                                &decoded_samples,
                                &mut first_push_logged,
                                seek_closure(
                                    &mut *format,
                                    &mut *decoder,
                                    &mut pipeline,
                                    &decode_ctx,
                                ),
                            ) {
                                PushRun::Drained => {
                                    pending_tail.clear();
                                    let _ = event_tx.send(DecodeEvent::Finished);
                                }
                                PushRun::Stopped => {
                                    crate::vprintln!(
                                        "[DECODE] Stop received while parked, exiting"
                                    );
                                    return;
                                }
                                PushRun::Paused(rest) => {
                                    pending_tail = rest;
                                    paused = true;
                                }
                                // The reader left the position this tail belongs to, and the
                                // block that owns it ends with this loop.
                                PushRun::Moved => break,
                            }
                        }
                        Ok(DecodeCommand::Stop) => {
                            crate::vprintln!("[DECODE] Stop received while parked, exiting");
                            return;
                        }
                        Err(_) => return,
                    }
                }
                continue;
            }
            Err(symphonia::core::errors::Error::ResetRequired) => {
                decoder.reset();
                continue;
            }
            Err(e) => {
                // Asked for. Nothing here has failed and nothing needs announcing. The
                // handler for `Error` drops the track's cache entry, and the bytes behind a
                // stop are good; guessing wrong here costs a re-download of a track that
                // decoded perfectly, and a media error the listener never earned.
                if let Some(stop) = requested_stop(&e) {
                    crate::vprintln!("[DECODE] Stopped: {stop}");
                    let _ = event_tx.send(DecodeEvent::Stopped);
                    return;
                }
                // A dead network says so two ways: the READER gave up after thirty seconds
                // (`TimedOut`), or the WRITER gave up first and stored its failure
                // (`ConnectionAborted`). On a pulled cable the writer always wins that race,
                // eight reconnects with backoff against thirty seconds, so listening for the
                // timeout alone surfaced a cut network as "unexpected error (NPO03)".
                if let symphonia::core::errors::Error::IoError(ref io) = e
                    && matches!(
                        io.kind(),
                        std::io::ErrorKind::TimedOut | std::io::ErrorKind::ConnectionAborted
                    )
                {
                    crate::vprintln!("[DECODE] Network stalled: {e}");
                    let _ = event_tx.send(DecodeEvent::NetworkStalled);
                    return;
                }
                let _ = event_tx.send(DecodeEvent::Error(format!("packet error: {e}")));
                let _ = event_tx.send(DecodeEvent::Finished);
                return;
            }
        };

        if packet.track_id != track_id {
            continue;
        }

        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(symphonia::core::errors::Error::DecodeError(e)) => {
                crate::vprintln!("[DECODE] decode error (skipping): {e}");
                continue;
            }
            Err(e) => {
                let _ = event_tx.send(DecodeEvent::Error(format!("decode fatal: {e}")));
                let _ = event_tx.send(DecodeEvent::Finished);
                return;
            }
        };

        let num_frames = decoded.frames();

        sample_vec.clear();
        decoded.copy_to_vec_interleaved::<f32>(&mut sample_vec);
        let source_samples = sample_vec.as_slice();

        if !first_packet_logged {
            first_packet_logged = true;
            crate::vprintln!(
                "[DECODE] First packet: {} frames, {} source samples",
                num_frames,
                source_samples.len()
            );
        }

        let resampled: Vec<f32>;
        let samples_to_push: &[f32] = if let Some(ref mut pipe) = pipeline {
            resampled = match pipe.process(source_samples) {
                Ok(s) => s,
                Err(e) => {
                    let _ = event_tx.send(DecodeEvent::Error(e));
                    let _ = event_tx.send(DecodeEvent::Finished);
                    return;
                }
            };
            if !first_push_logged && !resampled.is_empty() {
                let (min, max) = resampled
                    .iter()
                    .fold((f32::MAX, f32::MIN), |(mn, mx), &s| (mn.min(s), mx.max(s)));
                crate::vprintln!(
                    "[DECODE] First resampled: {} samples | min={:.6} max={:.6}",
                    resampled.len(),
                    min,
                    max
                );
            }
            &resampled
        } else {
            if !first_push_logged && !source_samples.is_empty() {
                let (min, max) = source_samples
                    .iter()
                    .fold((f32::MAX, f32::MIN), |(mn, mx), &s| (mn.min(s), mx.max(s)));
                crate::vprintln!(
                    "[DECODE] First output (passthrough): {} samples | min={:.6} max={:.6}",
                    source_samples.len(),
                    min,
                    max
                );
            }
            source_samples
        };

        match push_until_settled(
            samples_to_push,
            &mut producer,
            &cmd_rx,
            &decoded_samples,
            &mut first_push_logged,
            seek_closure(&mut *format, &mut *decoder, &mut pipeline, &decode_ctx),
        ) {
            PushRun::Drained => {}
            PushRun::Stopped => return,
            // The reader is already past this packet and the pipeline has consumed its
            // output: what the pause cut off is carried to the top of the loop. Decoding
            // on without it is what shortened the track by up to a packet per pause.
            PushRun::Paused(rest) => {
                carried_tail = rest;
                paused = true;
            }
            PushRun::Moved => {}
        }
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/player/thread/decode.rs"]
mod tests;
