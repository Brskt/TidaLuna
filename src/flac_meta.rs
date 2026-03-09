/// Minimal FLAC SEEKTABLE parser.
///
/// Extracts seek points from the SEEKTABLE metadata block so the streaming
/// layer can predict where Symphonia's binary search will read and set the
/// Range-request padding accordingly.

#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct FlacSeekPoint {
    pub sample_number: u64,
    /// Byte offset of the target frame, **relative to the first audio frame**.
    pub stream_offset: u64,
    pub frame_samples: u16,
}

#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct FlacSeekInfo {
    /// Absolute byte position where audio frames begin (after all metadata blocks).
    pub first_frame_offset: u64,
    pub seek_points: Vec<FlacSeekPoint>,
}

#[allow(dead_code)]
impl FlacSeekInfo {
    /// Parse FLAC metadata from raw bytes (must start at byte 0 of the file).
    /// Returns `None` if the data is too short, not FLAC, or has no SEEKTABLE.
    pub fn parse(data: &[u8]) -> Option<Self> {
        if data.len() < 8 || &data[0..4] != b"fLaC" {
            return None;
        }

        let mut pos: usize = 4;
        let mut seek_points = Vec::new();

        loop {
            if pos + 4 > data.len() {
                break;
            }
            let is_last = (data[pos] & 0x80) != 0;
            let block_type = data[pos] & 0x7F;
            let length = ((data[pos + 1] as usize) << 16)
                | ((data[pos + 2] as usize) << 8)
                | (data[pos + 3] as usize);
            pos += 4;

            if block_type == 3 {
                // SEEKTABLE — entries of 18 bytes each
                let entry_count = length / 18;
                let end = pos + entry_count * 18;
                if end > data.len() {
                    break;
                }
                seek_points.reserve(entry_count);
                for i in 0..entry_count {
                    let base = pos + i * 18;
                    let sample_number = u64::from_be_bytes(
                        data[base..base + 8]
                            .try_into()
                            .expect("18-byte entry bounds checked"),
                    );
                    // Placeholder entries have sample_number == 0xFFFFFFFFFFFFFFFF
                    if sample_number == u64::MAX {
                        continue;
                    }
                    let stream_offset = u64::from_be_bytes(
                        data[base + 8..base + 16]
                            .try_into()
                            .expect("18-byte entry bounds checked"),
                    );
                    let frame_samples = u16::from_be_bytes(
                        data[base + 16..base + 18]
                            .try_into()
                            .expect("18-byte entry bounds checked"),
                    );
                    seek_points.push(FlacSeekPoint {
                        sample_number,
                        stream_offset,
                        frame_samples,
                    });
                }
            }

            pos += length;
            if is_last {
                break;
            }
        }

        if seek_points.is_empty() {
            return None;
        }

        Some(FlacSeekInfo {
            first_frame_offset: pos as u64,
            seek_points,
        })
    }

    /// Convert a seek time to the approximate byte offset symphonia will target.
    ///
    /// Returns the midpoint between the SEEKTABLE lower and upper bounds for the
    /// given time, matching symphonia's binary search first probe position.
    /// Returns `None` if SEEKTABLE is empty or sample_rate is 0.
    pub fn byte_offset_for_time(&self, seconds: f64, sample_rate: u32) -> Option<u64> {
        if self.seek_points.is_empty() || sample_rate == 0 {
            return None;
        }
        let target_sample = (seconds * sample_rate as f64) as u64;

        // Find the last seek point with sample_number <= target_sample
        let idx = match self
            .seek_points
            .binary_search_by_key(&target_sample, |sp| sp.sample_number)
        {
            Ok(i) => i,
            Err(0) => 0,
            Err(i) => i - 1,
        };

        let lower = self.first_frame_offset + self.seek_points[idx].stream_offset;
        let upper = if idx + 1 < self.seek_points.len() {
            self.first_frame_offset + self.seek_points[idx + 1].stream_offset
        } else {
            // Last seek point — no upper bound, return just the lower
            return Some(lower);
        };

        // Midpoint matches symphonia's binary search first probe
        Some(lower / 2 + upper / 2)
    }

    /// Find the `[lower, upper]` absolute byte offsets bounding `target_byte`.
    ///
    /// Symphonia's binary search will seek to roughly `(lower + upper) / 2` on
    /// its first iteration, so the caller should ensure the Range request
    /// covers at least that midpoint.
    pub fn byte_range_for(&self, target_byte: u64) -> Option<(u64, u64)> {
        if self.seek_points.is_empty() {
            return None;
        }

        // Convert seek point stream_offsets to absolute file offsets
        let target_rel = target_byte.checked_sub(self.first_frame_offset)?;

        // Binary search: find the last seek point with stream_offset <= target_rel
        let idx = match self
            .seek_points
            .binary_search_by_key(&target_rel, |sp| sp.stream_offset)
        {
            Ok(i) => i,
            Err(0) => return None, // target is before the first seek point
            Err(i) => i - 1,
        };

        let lower = self.first_frame_offset + self.seek_points[idx].stream_offset;
        let upper = if idx + 1 < self.seek_points.len() {
            self.first_frame_offset + self.seek_points[idx + 1].stream_offset
        } else {
            // Last seek point — upper bound is unknown, use target itself
            target_byte
        };

        Some((lower, upper))
    }
}
