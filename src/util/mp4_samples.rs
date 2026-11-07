//! The two checks every read of an MP4's samples goes through, whatever the file feeds.

/// The bytes of one sample, or `None` for an empty range or one this buffer does not contain.
///
/// `data_offset` and `data_size` are read out of `stco`/`co64`/`stsz` or `trun`, which is
/// untrusted input. `shiguredo_mp4` validates them against each other and against integer
/// overflow, but never against the length of the buffer the caller holds: its own documentation
/// example slices directly on them and panics on a truncated file. This is that missing
/// check, and it is the reason every sample read goes through here.
pub(crate) fn sample_bytes(file: &[u8], offset: u64, size: usize) -> Option<&[u8]> {
    if size == 0 {
        return None;
    }
    let start = usize::try_from(offset).ok()?;
    let end = start.checked_add(size)?;
    file.get(start..end)
}

/// What one track's samples may claim, charged as each one arrives. Two ceilings, neither
/// implying the other:
///
/// * the COUNT bounds what the tables may declare: `stts` is run-length encoded and `stsz` in
///   fixed mode stores only a count (a few bytes can ask for billions of samples);
/// * the BYTE TOTAL bounds what the samples may point at. The demuxer checks offsets for
///   overflow only, never for overlap: every chunk may name one region, each sample kept as its
///   own copy, and neither the count nor the download cap stops that aliasing.
///
/// The byte ceiling is the file's length: a well-formed track's samples occupy disjoint regions
/// of it, and aliasing is what exceeds it. Both refuse rather than truncate.
pub(crate) struct SampleBudget {
    what: &'static str,
    max_count: usize,
    file_len: usize,
    claimed: usize,
    count: usize,
}

impl SampleBudget {
    /// `what` names the file in the refusal, as in "cover" or "DASH track".
    pub(crate) fn new(what: &'static str, max_count: usize, file_len: usize) -> Self {
        Self {
            what,
            max_count,
            file_len,
            claimed: 0,
            count: 0,
        }
    }

    /// Charged per sample, before anything is copied: a total computed up front would be the
    /// hostile value itself.
    pub(crate) fn charge(&mut self, data_size: usize) -> anyhow::Result<()> {
        let what = self.what;
        if self.count >= self.max_count {
            anyhow::bail!("{what} exceeds the ceiling of {} samples", self.max_count);
        }
        let claimed = self.claimed.saturating_add(data_size);
        if claimed > self.file_len {
            anyhow::bail!(
                "{what} samples claim {claimed} bytes of a {} byte file; refusing it",
                self.file_len
            );
        }
        self.claimed = claimed;
        self.count += 1;
        Ok(())
    }
}

#[cfg(test)]
#[path = "../../tests/unit/util/mp4_samples.rs"]
mod tests;
