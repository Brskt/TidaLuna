use rubato::Resampler;

/// Source frames the resampler takes per call. Shared with the tests, which size their
/// fixtures against it: a private literal would let the two drift apart in silence.
pub(super) const CHUNK_SIZE: usize = 1024;

pub(in crate::player::thread) struct AudioPipeline {
    /// Absent when source and output share a rate: a sinc filter at ratio 1.0 returns
    /// samples that were already correct, minus a low-pass and a lead-in delay. Rate parity
    /// gets the channel remap alone.
    resampler: Option<rubato::Async<f32>>,
    source_channels: usize,
    output_channels: usize,
    accum: Vec<Vec<f32>>,
    accum_frames: usize,
}

impl AudioPipeline {
    /// Fails rather than panics: the rate pair comes from a container header and a device,
    /// neither of which this thread controls, and a panic here takes the decode thread down
    /// without a `Finished` or an `Error` for the player to act on.
    pub fn new(
        source_rate: u32,
        output_rate: u32,
        source_channels: usize,
        output_channels: usize,
    ) -> Result<Self, String> {
        let resampler = if source_rate == output_rate {
            None
        } else {
            let params = rubato::SincInterpolationParameters {
                sinc_len: 256,
                f_cutoff: Some(0.95),
                interpolation: rubato::SincInterpolationType::Linear,
                oversampling_factor: 256,
                window: rubato::WindowFunction::BlackmanHarris2,
            };
            Some(
                rubato::Async::<f32>::new_sinc(
                    output_rate as f64 / source_rate as f64,
                    2.0,
                    &params,
                    CHUNK_SIZE,
                    source_channels,
                    rubato::FixedAsync::Input,
                )
                .map_err(|e| format!("resampler {source_rate} -> {output_rate}: {e}"))?,
            )
        };

        // The accumulator stages frames for the resampler and has no second reader: rate parity
        // leaves it unbuilt rather than holding a buffer `process` and `flush` both skip.
        let accum = if resampler.is_some() {
            vec![Vec::with_capacity(CHUNK_SIZE * 2); source_channels]
        } else {
            Vec::new()
        };

        Ok(Self {
            resampler,
            source_channels,
            output_channels,
            accum,
            accum_frames: 0,
        })
    }

    pub fn resamples(&self) -> bool {
        self.resampler.is_some()
    }

    /// Errors instead of dropping the chunk: a gated log paired with an unconditional drain
    /// lets a whole track vanish under normal-looking playback.
    pub fn process(&mut self, interleaved: &[f32]) -> Result<Vec<f32>, String> {
        use audioadapter_buffers::direct::SequentialSliceOfVecs;

        let Self {
            resampler,
            source_channels,
            output_channels,
            accum,
            accum_frames,
        } = self;
        let (src_ch, out_ch) = (*source_channels, *output_channels);

        let Some(resampler) = resampler.as_mut() else {
            let mut output = Vec::new();
            remap_channels(interleaved, src_ch, out_ch, &mut output);
            return Ok(output);
        };

        let frames = interleaved.len() / src_ch;
        for f in 0..frames {
            for ch in 0..src_ch {
                accum[ch].push(interleaved[f * src_ch + ch]);
            }
        }
        *accum_frames += frames;

        let mut output = Vec::new();
        while *accum_frames >= CHUNK_SIZE {
            let data = {
                let adapter = SequentialSliceOfVecs::new(accum, src_ch, CHUNK_SIZE)
                    .map_err(|e| format!("resampler input rejected: {e}"))?;
                resampler
                    .process(&adapter, None)
                    .map_err(|e| format!("resampling failed: {e}"))?
                    .take_data()
            };
            remap_channels(&data, src_ch, out_ch, &mut output);

            for ch in accum.iter_mut() {
                ch.drain(..CHUNK_SIZE);
            }
            *accum_frames -= CHUNK_SIZE;
        }

        Ok(output)
    }

    pub fn flush(&mut self) -> Result<Vec<f32>, String> {
        use audioadapter_buffers::direct::SequentialSliceOfVecs;
        use rubato::Indexing;

        let Self {
            resampler,
            source_channels,
            output_channels,
            accum,
            accum_frames,
        } = self;
        let (src_ch, out_ch) = (*source_channels, *output_channels);

        // Nothing accumulates without a resampler: `process` remaps and returns in one go.
        let Some(resampler) = resampler.as_mut() else {
            return Ok(Vec::new());
        };
        if *accum_frames == 0 {
            return Ok(Vec::new());
        }

        let partial_frames = *accum_frames;
        for ch in accum.iter_mut() {
            ch.resize(CHUNK_SIZE, 0.0);
        }

        let out_max = resampler.output_frames_max();
        let mut out_buf =
            audioadapter_buffers::owned::InterleavedOwned::<f32>::new(0.0, src_ch, out_max);

        let indexing = Indexing {
            input_offset: 0,
            output_offset: 0,
            partial_len: Some(partial_frames),
            active_channels_mask: None,
        };

        let written = {
            let adapter = SequentialSliceOfVecs::new(accum, src_ch, CHUNK_SIZE)
                .map_err(|e| format!("resampler input rejected: {e}"))?;
            let (_consumed, written) = resampler
                .process_into_buffer(&adapter, &mut out_buf, Some(&indexing))
                .map_err(|e| format!("flushing the resampler failed: {e}"))?;
            written
        };

        let mut output = Vec::new();
        let data = out_buf.take_data();
        remap_channels(&data[..written * src_ch], src_ch, out_ch, &mut output);

        for ch in accum.iter_mut() {
            ch.clear();
        }
        *accum_frames = 0;
        Ok(output)
    }

    pub fn reset(&mut self) {
        for ch in &mut self.accum {
            ch.clear();
        }
        self.accum_frames = 0;
        if let Some(resampler) = self.resampler.as_mut() {
            // The sinc history holds up to `sinc_len` frames of pre-seek audio; leaving it
            // loaded bleeds them into the first output after the seek.
            resampler.reset();
        }
    }
}

/// Free function rather than a method: `process` holds a split borrow of the accumulator
/// and the resampler, which a `&self` receiver would conflict with.
fn remap_channels(data: &[f32], src_ch: usize, out_ch: usize, output: &mut Vec<f32>) {
    let frames = data.len() / src_ch;
    output.reserve(frames * out_ch);

    if src_ch == out_ch {
        output.extend_from_slice(data);
        return;
    }

    for f_idx in 0..frames {
        for ch in 0..out_ch {
            let sample = if ch < src_ch {
                data[f_idx * src_ch + ch]
            } else if src_ch == 1 {
                data[f_idx * src_ch] // mono -> multi: duplicate
            } else {
                0.0 // extra channels: silence
            };
            output.push(sample);
        }
    }
}

#[cfg(test)]
#[path = "../../../../tests/unit/player/thread/output/resample.rs"]
mod tests;
