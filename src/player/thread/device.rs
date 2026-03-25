use super::decode::{DecodeThreadConfig, spawn_decode_thread};
use super::output::{find_output_device, open_output_stream};
use super::{DecodeCommand, PlayerThread};
use crate::player::buffer::RamBuffer;
use crate::player::{DeviceErrorKind, PlayerEvent};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::mpsc;

use cpal::traits::HostTrait;
use symphonia::core::codecs::CODEC_TYPE_NULL;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

#[cfg(target_os = "windows")]
use crate::player::{EXCLUSIVE_STREAM_SEQ, wasapi};
#[cfg(target_os = "windows")]
use std::sync::Arc;
#[cfg(target_os = "windows")]
use std::sync::atomic::AtomicBool;
#[cfg(target_os = "windows")]
use std::thread;
#[cfg(target_os = "windows")]
use wasapi::{ExclusiveCommand, ExclusiveHandle};

impl<F: Fn(PlayerEvent) + Send + 'static> PlayerThread<F> {
    pub(super) fn handle_set_audio_device(&mut self, id: String, exclusive: bool) {
        let _ = &exclusive;
        #[cfg(target_os = "windows")]
        {
            if let Some(cancel) = self.exclusive_stream_cancel.take() {
                cancel.store(true, Relaxed);
            }
            if exclusive {
                self.stop_decode();
                self.cpal_stream = None;

                if let Some(old) = self.exclusive_handle.take() {
                    old.shutdown();
                }

                let handle = ExclusiveHandle::spawn(id.clone());
                self.is_exclusive_mode = true;
                self.exclusive_handle = Some(handle);

                if let Some(ref handle) = self.exclusive_handle
                    && let Some(ref buf) = self.current_buffer
                {
                    handle.send(ExclusiveCommand::Stop);
                    let cancel = Arc::new(AtomicBool::new(false));
                    self.exclusive_stream_cancel = Some(cancel.clone());

                    let cmd_tx = handle.command_sender();
                    let reader = buf.clone();
                    let total_len = buf.total_len();
                    let stream_id = EXCLUSIVE_STREAM_SEQ.fetch_add(1, Relaxed) + 1;
                    thread::spawn(move || {
                        if let Err(e) = wasapi::stream_flac_reader_to_wasapi(
                            reader,
                            total_len,
                            stream_id,
                            cmd_tx,
                            cancel.clone(),
                        ) && !cancel.load(Relaxed)
                        {
                            eprintln!("[WASAPI] Device switch stream decode failed: {e}");
                        }
                    });
                    self.has_track = true;
                    self.is_playing = true;
                }

                self.current_device_id = Some(id.clone());
                crate::vprintln!("[AUDIO] Switched to exclusive WASAPI: {}", id);
                return;
            } else if self.is_exclusive_mode {
                if let Some(old) = self.exclusive_handle.take() {
                    old.shutdown();
                }
                self.is_exclusive_mode = false;
                crate::vprintln!("[AUDIO] Switched back to shared mode");
            }
        }

        self.current_device_id = Some(id.clone());

        if self.has_track {
            let was_playing = self.is_playing;
            let position = self.current_position_secs();

            self.stop_decode();
            self.cpal_stream = None;

            if let Some(ref buffer) = self.current_buffer {
                let buffer_clone = buffer.clone();
                self.rebuild_pipeline_on_device(buffer_clone, was_playing, position);
            }
        }

        crate::vprintln!("[AUDIO] Switched to device: {}", id);
    }

    pub(super) fn rebuild_pipeline_on_device(
        &mut self,
        buffer: RamBuffer,
        was_playing: bool,
        seek_to: f64,
    ) {
        let _total_len = buffer.total_len();

        let probe_reader = buffer.clone();
        let mss = MediaSourceStream::new(Box::new(probe_reader), Default::default());
        let hint = Hint::new();

        let probed = match symphonia::default::get_probe().format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        ) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("[ERROR]  Probe failed on device switch: {e}");
                return;
            }
        };

        let track = match probed
            .format
            .tracks()
            .iter()
            .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        {
            Some(t) => t,
            None => {
                eprintln!("[ERROR]  No audio track on device switch");
                return;
            }
        };

        let sr = track.codec_params.sample_rate.unwrap_or(44100);
        let ch = track
            .codec_params
            .channels
            .map(|c| c.count() as u16)
            .unwrap_or(2);

        drop(probed);

        let device = if let Some(ref id) = self.current_device_id {
            match find_output_device(id) {
                Some(d) => d,
                None => {
                    crate::vprintln!("[AUDIO] Device '{}' not found, falling back to default", id);
                    (self.callback)(PlayerEvent::DeviceError(DeviceErrorKind::NotFound));
                    match cpal::default_host().default_output_device() {
                        Some(d) => d,
                        None => {
                            (self.callback)(PlayerEvent::DeviceError(DeviceErrorKind::NotFound));
                            return;
                        }
                    }
                }
            }
        } else {
            match cpal::default_host().default_output_device() {
                Some(d) => d,
                None => {
                    (self.callback)(PlayerEvent::DeviceError(DeviceErrorKind::NotFound));
                    return;
                }
            }
        };

        let opened = match open_output_stream(&device, sr, ch, &self.volume) {
            Some(o) => o,
            None => {
                (self.callback)(PlayerEvent::DeviceError(
                    DeviceErrorKind::FormatNotSupported,
                ));
                return;
            }
        };

        let actual_rate = opened.rate;
        let actual_channels = opened.channels;
        self.cpal_muted = Some(opened.muted);
        self.cpal_stream_error = Some(opened.stream_error);
        self.played_samples = opened.played_samples;

        self.sample_rate = actual_rate;
        self.channels = actual_channels;
        self.decoded_samples.store(0, Relaxed);
        self.played_samples.store(0, Relaxed);

        let (decode_cmd_tx, decode_cmd_rx) = mpsc::channel();
        let (decode_event_tx, decode_event_rx) = mpsc::channel();
        let decoded_samples = self.decoded_samples.clone();

        let decode_buffer = buffer.clone();
        let decode_handle = spawn_decode_thread(DecodeThreadConfig {
            buffer: decode_buffer,
            producer: opened.producer,
            decoded_samples,
            cmd_rx: decode_cmd_rx,
            event_tx: decode_event_tx,
            output_rate: actual_rate,
            output_channels: actual_channels,
            seek_gen: opened.seek_gen,
        });

        let stream = opened.stream;

        self.cpal_stream = Some(stream);
        self.decode_cmd_tx = Some(decode_cmd_tx);
        self.decode_event_rx = Some(decode_event_rx);
        self.decode_handle = Some(decode_handle);
        self.current_buffer = Some(buffer);

        if seek_to > 0.0
            && let Some(ref tx) = self.decode_cmd_tx
        {
            let _ = tx.send(DecodeCommand::Seek(seek_to));
        }

        if was_playing {
            self.start_playback();
        }
    }
}
