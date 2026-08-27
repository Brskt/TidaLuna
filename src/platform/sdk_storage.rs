// FRAGILE: depends on Chromium LevelDB localStorage internals + TIDAL SDK crypto scheme.
// Revalidate on Chromium/CEF upgrades (LevelDB key/value format) and TIDAL SDK updates
// (PBKDF2 params, AES-KW, AES-CTR counter_bits, localStorage key names).
//
// Fail-closed: read_raw_blob() returns Missing (no DB / no keys), Corrupt (present but
// malformed), Unreadable, or Raw; decrypt_raw_blob() returns None on any crypto or
// schema failure. Never panics, never returns partial results.
//
// I/O and crypto are split on purpose: the LevelDB halves (read_raw_blob,
// write_entries, purge_sdk_credentials) must only run before CEF init - Chromium
// locks that directory once it starts - while the pure halves (decrypt_raw_blob,
// build_seed_entries) carry the 100k-iteration PBKDF2 and may run on any thread.

use aes::Aes256;
use aes::cipher::{KeyInit, KeyIvInit, StreamCipher};
use serde::Deserialize;
use std::path::Path;

type Aes256Ctr64 = ctr::Ctr64BE<Aes256>;

const ORIGIN: &str = "https://desktop.tidal.com";
const PBKDF2_ITERATIONS: u32 = 100_000;
const PBKDF2_KEY_LEN: usize = 32; // 256 bits
const AES_KW_WRAPPED_LEN: usize = 40; // 32-byte key + 8-byte integrity check
const AES_KW_UNWRAPPED_LEN: usize = 32;
const COUNTER_LEN: usize = 16;
const SALT_LEN: usize = 16;
const PASSWORD: &[u8] = b"tidal";

/// Build a Chrome localStorage LevelDB key.
/// Format: `_` + origin + `\x00` + encoding_prefix + key_name
fn leveldb_key(ls_key: &str) -> Vec<u8> {
    let mut key = Vec::with_capacity(1 + ORIGIN.len() + 1 + 1 + ls_key.len());
    key.push(b'_');
    key.extend_from_slice(ORIGIN.as_bytes());
    key.push(0x00);
    key.push(0x01); // ISO-8859-1 encoding
    key.extend_from_slice(ls_key.as_bytes());
    key
}

/// Decode a Chrome localStorage value.
/// First byte = encoding (0x01 = Latin-1 bytes, 0x00 = UTF-16-LE).
/// Returns raw bytes (the char codes).
fn decode_ls_value(raw: &[u8]) -> Option<Vec<u8>> {
    if raw.is_empty() {
        return None;
    }
    match raw[0] {
        0x01 => Some(raw[1..].to_vec()),
        0x00 => {
            let payload = &raw[1..];
            if !payload.len().is_multiple_of(2) {
                return None;
            }
            Some(
                payload
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|pair| pair[0])
                    .collect(),
            )
        }
        _ => None,
    }
}

/// Derive the AES-256 wrapping key from the password + salt via PBKDF2-HMAC-SHA256.
fn derive_wrapping_key(salt: &[u8; SALT_LEN]) -> [u8; PBKDF2_KEY_LEN] {
    let mut key = [0u8; PBKDF2_KEY_LEN];
    pbkdf2::pbkdf2_hmac::<sha2::Sha256>(PASSWORD, salt, PBKDF2_ITERATIONS, &mut key);
    key
}

/// Unwrap the AES-256 data key using AES-KW (RFC 3394).
fn unwrap_data_key(
    wrapping_key: &[u8; PBKDF2_KEY_LEN],
    wrapped: &[u8],
) -> Option<[u8; AES_KW_UNWRAPPED_LEN]> {
    if wrapped.len() != AES_KW_WRAPPED_LEN {
        return None;
    }
    let kek = aes_kw::KwAes256::new_from_slice(wrapping_key).expect("key is 32 bytes");
    let mut buf = [0u8; AES_KW_UNWRAPPED_LEN];
    kek.unwrap_key(wrapped, &mut buf).ok()?;
    Some(buf)
}

/// Decrypt data via AES-256-CTR with counter_bits=64.
/// counter[0..8] = fixed nonce, counter[8..16] = 64-bit BE counter.
fn decrypt_aes_ctr(
    data_key: &[u8; AES_KW_UNWRAPPED_LEN],
    counter: &[u8; COUNTER_LEN],
    ciphertext: &[u8],
) -> Option<Vec<u8>> {
    let mut cipher = Aes256Ctr64::new_from_slices(data_key, counter).ok()?;
    let mut plaintext = ciphertext.to_vec();
    cipher.apply_keystream(&mut plaintext);
    Some(plaintext)
}

fn encrypt_aes_ctr(
    data_key: &[u8; AES_KW_UNWRAPPED_LEN],
    counter: &[u8; COUNTER_LEN],
    plaintext: &[u8],
) -> Option<Vec<u8>> {
    decrypt_aes_ctr(data_key, counter, plaintext)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SdkCredentials {
    pub access_token: Option<SdkAccessToken>,
    pub refresh_token: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SdkAccessToken {
    pub token: Option<String>,
}

/// The four AuthDB/tidal* values, read raw from LevelDB before any crypto.
pub(crate) struct RawSdkBlob {
    pub salt: [u8; SALT_LEN],
    pub counter: [u8; COUNTER_LEN],
    pub wrapped_key: Vec<u8>,
    pub data: Vec<u8>,
}

pub(crate) enum ReadRawResult {
    Missing,
    Raw(Box<RawSdkBlob>),
    Corrupt,
    /// Could not open the LevelDB (e.g. locked by another process); contents
    /// unknown: callers must not purge. It may hold valid credentials.
    Unreadable,
}

/// I/O half of the read path: fetch the four keys and validate their shape.
/// No crypto happens here; the blob decrypts later via decrypt_raw_blob.
pub(crate) fn read_raw_blob(leveldb_path: &Path) -> ReadRawResult {
    let mut db = match open_leveldb(leveldb_path) {
        OpenResult::Missing => return ReadRawResult::Missing,
        OpenResult::Ok(db) => *db,
        OpenResult::Error => return ReadRawResult::Unreadable,
    };

    let keys = [
        read_ls_key(&mut db, "AuthDB/tidalSalt"),
        read_ls_key(&mut db, "AuthDB/tidalCounter"),
        read_ls_key(&mut db, "AuthDB/tidalKey"),
        read_ls_key(&mut db, "AuthDB/tidalData"),
    ];

    let mut present = 0u8;
    let mut has_invalid = false;
    for result in &keys {
        match result {
            ReadKeyResult::Ok(_) => present += 1,
            ReadKeyResult::Missing => {}
            ReadKeyResult::Invalid => {
                present += 1;
                has_invalid = true;
            }
        }
    }

    if present == 0 {
        return ReadRawResult::Missing;
    }
    if present < 4 || has_invalid {
        return ReadRawResult::Corrupt;
    }

    let [salt_key, counter_key, wrapped_key, data_key] = keys;
    let (
        ReadKeyResult::Ok(salt_bytes),
        ReadKeyResult::Ok(counter_bytes),
        ReadKeyResult::Ok(wrapped_bytes),
        ReadKeyResult::Ok(data_bytes),
    ) = (salt_key, counter_key, wrapped_key, data_key)
    else {
        return ReadRawResult::Corrupt;
    };

    let Ok(salt) = <[u8; SALT_LEN]>::try_from(salt_bytes.as_slice()) else {
        return ReadRawResult::Corrupt;
    };
    let Ok(counter) = <[u8; COUNTER_LEN]>::try_from(counter_bytes.as_slice()) else {
        return ReadRawResult::Corrupt;
    };

    ReadRawResult::Raw(Box::new(RawSdkBlob {
        salt,
        counter,
        wrapped_key: wrapped_bytes,
        data: data_bytes,
    }))
}

/// Crypto half of the read path: PBKDF2, AES-KW unwrap, AES-CTR decrypt, JSON
/// parse. Pure CPU, no I/O. None on any failure (the blob is corrupt).
pub(crate) fn decrypt_raw_blob(raw: &RawSdkBlob) -> Option<SdkCredentials> {
    let wrapping_key = derive_wrapping_key(&raw.salt);
    let data_key = unwrap_data_key(&wrapping_key, &raw.wrapped_key)?;
    let plaintext = decrypt_aes_ctr(&data_key, &raw.counter, &raw.data)?;
    let json_str = std::str::from_utf8(&plaintext).ok()?;
    serde_json::from_str::<SdkCredentials>(json_str).ok()
}

enum OpenResult {
    Missing,
    Ok(Box<rusty_leveldb::DB>),
    Error,
}

/// Chromium writes with Snappy (compressor id=1). Default CompressorList includes both
/// NoneCompressor (0) and SnappyCompressor (1); reads work. compressor=1 for writes.
fn open_leveldb(path: &Path) -> OpenResult {
    if !path.exists() {
        return OpenResult::Missing;
    }
    let opts = rusty_leveldb::Options {
        create_if_missing: false,
        compressor: 1, // Snappy, same as Chromium
        ..rusty_leveldb::Options::default()
    };
    match rusty_leveldb::DB::open(path, opts) {
        Ok(db) => OpenResult::Ok(Box::new(db)),
        Err(_) => OpenResult::Error,
    }
}

enum ReadKeyResult {
    Missing,
    Ok(Vec<u8>),
    Invalid,
}

fn read_ls_key(db: &mut rusty_leveldb::DB, ls_key: &str) -> ReadKeyResult {
    let key = leveldb_key(ls_key);
    let Some(raw) = db.get(&key) else {
        return ReadKeyResult::Missing;
    };
    match decode_ls_value(&raw) {
        Some(bytes) => ReadKeyResult::Ok(bytes),
        None => ReadKeyResult::Invalid,
    }
}

fn encode_ls_value(value: &[u8]) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(1 + value.len());
    encoded.push(0x01);
    encoded.extend_from_slice(value);
    encoded
}

fn write_ls_keys(db: &mut rusty_leveldb::DB, entries: &[(&str, &[u8])]) -> Option<()> {
    let mut batch = rusty_leveldb::WriteBatch::default();
    for (ls_key, value) in entries {
        let key = leveldb_key(ls_key);
        let encoded = encode_ls_value(value);
        batch.put(&key, &encoded);
    }
    db.write(batch, true).ok()?;
    Some(())
}

/// Encrypted write entries for a fresh credential blob: new salt, data key and
/// counter, including the full PBKDF2 derivation. Pure CPU, no I/O.
/// Used when the secure store has tokens but the SDK storage is missing
/// (TIDAL's SDK may refuse to persist non-JWT opaque tokens: it is seeded
/// with the real ones).
///
/// `expires_ms` is named for its unit because the SDK reads a milliseconds
/// epoch here and this side keeps seconds: `TokenGeneration::access_expires_ms`
/// is the conversion, and passing a raw `access_expires` would seed a token the
/// SDK judges expired on sight.
pub(crate) fn build_seed_entries(
    access_token: &str,
    refresh_token: &str,
    expires_ms: u64,
    user_id: Option<&str>,
    granted_scopes: &[String],
) -> Option<SdkEntries> {
    let mut salt = [0u8; SALT_LEN];
    getrandom::fill(&mut salt).ok()?;

    let mut data_key = [0u8; AES_KW_UNWRAPPED_LEN];
    getrandom::fill(&mut data_key).ok()?;

    let mut counter = [0u8; COUNTER_LEN];
    getrandom::fill(&mut counter).ok()?;

    let credentials_json = serde_json::json!({
        "accessToken": {
            "token": access_token,
            "expires": expires_ms,
            "userId": user_id.unwrap_or(""),
            "grantedScopes": granted_scopes,
        },
        "refreshToken": refresh_token,
    });
    let plaintext = serde_json::to_vec(&credentials_json).ok()?;

    let ciphertext = encrypt_aes_ctr(&data_key, &counter, &plaintext)?;

    let wrapping_key = derive_wrapping_key(&salt);
    let kek = aes_kw::KwAes256::new_from_slice(&wrapping_key).expect("key is 32 bytes");
    let mut wrapped_key = [0u8; AES_KW_WRAPPED_LEN];
    kek.wrap_key(&data_key, &mut wrapped_key).ok()?;

    Some(vec![
        ("AuthDB/tidalSalt", salt.to_vec()),
        ("AuthDB/tidalCounter", counter.to_vec()),
        ("AuthDB/tidalKey", wrapped_key.to_vec()),
        ("AuthDB/tidalData", ciphertext),
    ])
}

/// Pre-encrypted LevelDB write entries: (localStorage key, raw value bytes).
pub(crate) type SdkEntries = Vec<(&'static str, Vec<u8>)>;

/// Write pre-built entries to the blob, creating the LevelDB if absent (the
/// seed path can run on a fresh profile). Must run before CEF init.
pub(crate) fn write_entries(leveldb_path: &Path, entries: &SdkEntries) -> Option<()> {
    let opts = rusty_leveldb::Options {
        create_if_missing: true,
        compressor: 1,
        ..rusty_leveldb::Options::default()
    };
    let mut db = rusty_leveldb::DB::open(leveldb_path, opts).ok()?;
    let entry_refs: Vec<(&str, &[u8])> = entries.iter().map(|(k, v)| (*k, v.as_slice())).collect();
    write_ls_keys(&mut db, &entry_refs)
}

pub(crate) fn purge_sdk_credentials(leveldb_path: &Path) {
    let OpenResult::Ok(db) = open_leveldb(leveldb_path) else {
        return;
    };
    let mut db = *db;
    let mut batch = rusty_leveldb::WriteBatch::default();
    for suffix in ["Salt", "Counter", "Key", "Data"] {
        let key = leveldb_key(&format!("AuthDB/tidal{suffix}"));
        batch.delete(&key);
    }
    let _ = db.write(batch, true);
}
