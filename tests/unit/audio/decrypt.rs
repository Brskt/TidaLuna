//! Tests for `src/audio/decrypt.rs`, attached to it by `#[path]`.

use super::*;

/// Built from raw material, bypassing key-ID unwrapping: what matters here is
/// the CTR offset behaviour the disk cache relies on.
fn decryptor() -> FlacDecryptor {
    FlacDecryptor {
        key: [7u8; 16],
        nonce: [3u8; 8],
    }
}

/// The download decrypts chunk by chunk at running offsets, a cache hit decrypts
/// the whole file at offset 0, and cached playback is correct only if the two agree
/// byte for byte. The chunk size avoids a multiple of 16 to hit the partial-block skip.
#[test]
fn whole_file_decrypt_matches_streamed_chunk_decrypt() {
    let dec = decryptor();
    let ciphertext: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();

    let mut whole = ciphertext.clone();
    dec.decrypt_in_place(&mut whole, 0).unwrap();

    let mut streamed = Vec::with_capacity(ciphertext.len());
    let mut offset = 0u64;
    for chunk in ciphertext.chunks(777) {
        let mut buf = chunk.to_vec();
        dec.decrypt_in_place(&mut buf, offset).unwrap();
        offset += chunk.len() as u64;
        streamed.extend_from_slice(&buf);
    }

    assert_eq!(whole, streamed);
}
