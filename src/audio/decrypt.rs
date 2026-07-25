use aes::Aes128;
use aes::Aes256;
use aes::cipher::{BlockModeDecrypt, KeyIvInit, StreamCipher};
use base64::Engine;
use cbc::Decryptor as CbcDecryptor;
use ctr::Ctr128BE;
use tracing::debug;

const MASTER_KEY: &str = "UIlTTEMmmLfGowo/UC60x2H45W6MdGgTRfo/umg4754=";

#[derive(Clone, Copy)]
pub struct FlacDecryptor {
    key: [u8; 16],
    nonce: [u8; 8],
}

impl FlacDecryptor {
    fn decrypt_key_id(key_id_b64: &str) -> anyhow::Result<Self> {
        let master_key = base64::engine::general_purpose::STANDARD
            .decode(MASTER_KEY)
            .map_err(|e| anyhow::anyhow!("Failed to decode master key: {}", e))?;

        let key_id_bytes = base64::engine::general_purpose::STANDARD
            .decode(key_id_b64)
            .map_err(|e| anyhow::anyhow!("Failed to decode key ID: {}", e))?;

        if key_id_bytes.len() < 16 {
            anyhow::bail!("Key ID too short: need at least 16 bytes for IV");
        }

        let iv = &key_id_bytes[..16];
        let encrypted_key = &key_id_bytes[16..];

        debug!(
            "Decrypting key ID with AES-256-CBC (IV: {} bytes, encrypted: {} bytes)",
            iv.len(),
            encrypted_key.len()
        );

        type Aes256CbcDec = CbcDecryptor<Aes256>;
        let decryptor = Aes256CbcDec::new_from_slices(&master_key, iv)
            .map_err(|e| anyhow::anyhow!("Failed to create CBC decryptor: {}", e))?;

        let mut decrypted = encrypted_key.to_vec();
        let decrypted_key = decryptor
            .decrypt_padded::<aes::cipher::block_padding::Pkcs7>(&mut decrypted)
            .map_err(|e| anyhow::anyhow!("CBC decryption failed: {}", e))?;

        if decrypted_key.len() < 24 {
            anyhow::bail!(
                "Decrypted key too short: need at least 24 bytes (16 key + 8 nonce), got {}",
                decrypted_key.len()
            );
        }

        let key: [u8; 16] = decrypted_key[..16]
            .try_into()
            .expect("len >= 24 checked above");
        let nonce: [u8; 8] = decrypted_key[16..24]
            .try_into()
            .expect("len >= 24 checked above");

        debug!(
            "Extracted AES-128 key ({} bytes) and nonce ({} bytes)",
            key.len(),
            nonce.len()
        );

        Ok(Self { key, nonce })
    }

    pub fn new(encryption_key_b64: &str) -> anyhow::Result<Self> {
        if encryption_key_b64.is_empty() {
            anyhow::bail!("Empty encryption key");
        }

        debug!("Decrypting TIDAL key ID");
        Self::decrypt_key_id(encryption_key_b64)
    }

    fn build_iv_for_offset(&self, byte_offset: u64) -> [u8; 16] {
        let mut iv = [0u8; 16];
        iv[..8].copy_from_slice(&self.nonce);

        let block_number = byte_offset / 16;

        iv[8..16].copy_from_slice(&block_number.to_be_bytes());

        iv
    }

    /// Decrypt in-place: applies AES-128-CTR keystream directly on the mutable buffer.
    /// Avoids heap allocations - the caller's buffer IS the output.
    pub fn decrypt_in_place(&self, data: &mut [u8], byte_offset: u64) -> anyhow::Result<()> {
        if data.is_empty() {
            return Ok(());
        }

        debug!(
            "Decrypting {} bytes at offset {} with AES-128-CTR (in-place)",
            data.len(),
            byte_offset
        );

        let iv = self.build_iv_for_offset(byte_offset);

        type Aes128Ctr = Ctr128BE<Aes128>;
        let mut cipher = Aes128Ctr::new_from_slices(&self.key, &iv)
            .map_err(|e| anyhow::anyhow!("Failed to create CTR cipher: {}", e))?;

        let block_offset = (byte_offset % 16) as usize;
        if block_offset > 0 {
            // Advance the stream within the current block without heap allocation.
            let mut skip = [0u8; 16];
            cipher.apply_keystream(&mut skip[..block_offset]);
        }

        cipher.apply_keystream(data);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
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

    /// CTR XORs against a keystream, so the same call reverses itself. That is
    /// what makes storing ciphertext and decrypting on read lossless.
    #[test]
    fn decrypt_round_trips() {
        let dec = decryptor();
        let plain = b"fLaC and a payload that is not block aligned".to_vec();

        let mut buf = plain.clone();
        dec.decrypt_in_place(&mut buf, 0).unwrap();
        assert_ne!(
            buf, plain,
            "the keystream must actually transform the bytes"
        );

        dec.decrypt_in_place(&mut buf, 0).unwrap();
        assert_eq!(buf, plain);
    }

    /// A mid-file offset must not depend on having decrypted the earlier bytes -
    /// this is what would let a cached file be served from any point.
    #[test]
    fn a_mid_file_offset_decrypts_independently() {
        let dec = decryptor();
        let ciphertext: Vec<u8> = (0..2048u32).map(|i| (i % 97) as u8).collect();

        let mut whole = ciphertext.clone();
        dec.decrypt_in_place(&mut whole, 0).unwrap();

        let mut tail = ciphertext[1000..].to_vec();
        dec.decrypt_in_place(&mut tail, 1000).unwrap();

        assert_eq!(&whole[1000..], &tail[..]);
    }
}
