/// Does this look like a media container we could decode? Rejects a cache entry
/// whose key no longer fits it. Format-agnostic on purpose: the `format` string
/// comes from TIDAL's SDK and its vocabulary is not ours to enumerate.
fn looks_like_media(data: &[u8]) -> bool {
    if data.len() < 12 {
        return false;
    }
    data.starts_with(b"fLaC")
        || &data[4..8] == b"ftyp" // ISO BMFF: mp4 / m4a
        || data.starts_with(b"OggS")
        || data.starts_with(b"ID3")
        || looks_like_mpeg_frame(data)
}

/// Does `data` open on an ADTS-AAC frame header? Layer 00 with the 12th sync bit is a value MPEG
/// audio reserves; the two families cannot be confused. The frame's own length field must point at
/// a successor sync, which is what separates this from a bare sync word: 11 bits accept one random
/// byte pair in 2048.
fn looks_like_adts_frame(data: &[u8]) -> bool {
    if data.len() < 7 || data[0] != 0xFF {
        return false;
    }
    if data[1] & 0xF0 != 0xF0 || data[1] & 0x06 != 0 {
        return false;
    }
    // 13..15 are reserved rates; channel config 0 defers to an AudioSpecificConfig an ADTS
    // stream never carries.
    let sampling = (data[2] >> 2) & 0x0F;
    let channels = ((data[2] & 0x01) << 2) | (data[3] >> 6);
    if sampling > 12 || channels == 0 {
        return false;
    }
    // A frame cannot be shorter than the header it starts with.
    let frame_len =
        (((data[3] & 0x03) as usize) << 11) | ((data[4] as usize) << 3) | ((data[5] >> 5) as usize);
    if frame_len < 7 {
        return false;
    }
    match data.get(frame_len..frame_len + 2) {
        Some(next) => next[0] == 0xFF && next[1] & 0xF0 == 0xF0,
        // The successor lies past what we were given; the header alone stands.
        None => true,
    }
}

/// A cache-side plausibility check, not a security gate. It is built to reject accidental noise, not
/// an adversarial sequence, which is why `plugin.download` validates container structure instead. Raw
/// ADTS reaches here since symphonia's `aac` build probes with no hint; the MPEG-audio half stops at
/// the header because finding the next frame needs the bitrate tables.
fn looks_like_mpeg_frame(data: &[u8]) -> bool {
    if looks_like_adts_frame(data) {
        return true;
    }
    // Everything below reads the first two bytes, and the ADTS shape is already excluded by the
    // call above: layer 00 with the 12th sync bit is a value MPEG audio reserves.
    if data.len() < 7 || data[0] != 0xFF || (data[1] & 0xF0 == 0xF0 && data[1] & 0x06 == 0) {
        return false;
    }

    if data[1] & 0xE0 != 0xE0 {
        return false;
    }
    // Version 01, layer 00 and sampling 11 are reserved; bitrate 1111 is invalid and
    // 0000 is the free format nothing here emits. Finding the next frame would need
    // the bitrate tables; this stops at the header - what slips through is caught
    // by the decoder, which retires the entry.
    let version = (data[1] >> 3) & 0x03;
    let layer = (data[1] >> 1) & 0x03;
    let bitrate = (data[2] >> 4) & 0x0F;
    let sampling = (data[2] >> 2) & 0x03;
    version != 0x01 && layer != 0x00 && bitrate != 0x00 && bitrate != 0x0F && sampling != 0x03
}

/// Why a cache entry could not be turned into playable bytes. The distinction is
/// load-bearing: only `Corrupt` condemns the stored file.
pub(in crate::player) enum CacheReadError {
    /// The file is gone from disk: the index row is an orphan.
    Orphaned,
    /// Could not read the file, or could not use the key. Neither says anything about
    /// the stored ciphertext, and the entry must survive: evicting would discard a good
    /// file the next attempt would have to download again.
    Unreadable(String),
    /// Decrypted, but not into a container we recognise: the stored ciphertext no
    /// longer matches this key.
    Corrupt(String),
}

/// Read a cache entry and hand back playable bytes. Blocking and CPU-bound (a whole
/// file read plus an AES pass over every byte): callers run it off the runtime.
pub(in crate::player) fn read_cache_entry(
    path: &std::path::Path,
    key: &str,
) -> Result<Vec<u8>, CacheReadError> {
    let mut data = match std::fs::read(path) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(CacheReadError::Orphaned);
        }
        Err(e) => return Err(CacheReadError::Unreadable(format!("read failed: {e}"))),
    };

    // Key-side only: `new` unwraps the key ID and `decrypt_in_place` can fail solely
    // while building the cipher from it; neither outcome depends on the disk bytes.
    if !key.is_empty()
        && let Err(e) = crate::audio::decrypt::FlacDecryptor::new(key)
            .and_then(|d| d.decrypt_in_place(&mut data, 0))
    {
        return Err(CacheReadError::Unreadable(format!("unusable key: {e}")));
    }

    // AES-CTR is unauthenticated: a stale key decrypts to noise and still returns
    // Ok. This must run before `touch`, which would refresh the LRU position and keep
    // a bad entry from ever evicting.
    if !looks_like_media(&data) {
        return Err(CacheReadError::Corrupt(
            "not a known container after decrypt".to_string(),
        ));
    }

    Ok(data)
}

#[cfg(test)]
#[path = "../../../tests/unit/player/cache_entry_validation_tests.rs"]
mod cache_entry_validation_tests;
