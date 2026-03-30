// FRAGILE: depends on Chromium LevelDB localStorage internals + TIDAL SDK crypto scheme.
// Revalidate on Chromium/CEF upgrades (LevelDB key/value format) and TIDAL SDK updates
// (PBKDF2 params, AES-KW, AES-CTR counter_bits, localStorage key names).
//
// Fail-closed: read_sdk_credentials() returns Missing (no DB / no keys), Corrupt (present
// but unreadable), or Parsed. Never panics, never returns partial results.

use aes::Aes256;
use aes::cipher::{KeyIvInit, StreamCipher};
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
            Some(payload.chunks_exact(2).map(|pair| pair[0]).collect())
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
    let kek = aes_kw::KekAes256::new(wrapping_key.into());
    let mut buf = [0u8; AES_KW_UNWRAPPED_LEN];
    kek.unwrap(wrapped, &mut buf).ok()?;
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

pub(crate) struct CryptoParams {
    pub salt: [u8; SALT_LEN],
    pub wrapped_key: Vec<u8>,
    pub data_key: [u8; AES_KW_UNWRAPPED_LEN],
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
    pub expires: Option<u64>,
    pub user_id: Option<String>,
    pub client_id: Option<String>,
    pub granted_scopes: Option<Vec<String>>,
    pub requested_scopes: Option<Vec<String>>,
    pub client_unique_key: Option<String>,
}

pub(crate) enum ReadSdkResult {
    Missing,
    Parsed {
        credentials: Box<SdkCredentials>,
        crypto: Box<CryptoParams>,
        plaintext: Vec<u8>,
    },
    Corrupt,
}

pub(crate) fn read_sdk_credentials(leveldb_path: &Path) -> ReadSdkResult {
    let mut db = match open_leveldb(leveldb_path) {
        OpenResult::Missing => return ReadSdkResult::Missing,
        OpenResult::Ok(db) => *db,
        OpenResult::Error => return ReadSdkResult::Corrupt,
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
        return ReadSdkResult::Missing;
    }
    if present < 4 || has_invalid {
        return ReadSdkResult::Corrupt;
    }

    let mut iter = keys.into_iter().map(|r| match r {
        ReadKeyResult::Ok(bytes) => bytes,
        _ => unreachable!(),
    });
    let salt_bytes = iter.next().unwrap();
    let counter_bytes = iter.next().unwrap();
    let wrapped_bytes = iter.next().unwrap();
    let data_bytes = iter.next().unwrap();

    let Some(salt) = try_into_array::<SALT_LEN>(&salt_bytes) else {
        return ReadSdkResult::Corrupt;
    };
    let Some(counter) = try_into_array::<COUNTER_LEN>(&counter_bytes) else {
        return ReadSdkResult::Corrupt;
    };

    let wrapping_key = derive_wrapping_key(&salt);
    let Some(data_key) = unwrap_data_key(&wrapping_key, &wrapped_bytes) else {
        return ReadSdkResult::Corrupt;
    };
    let Some(plaintext) = decrypt_aes_ctr(&data_key, &counter, &data_bytes) else {
        return ReadSdkResult::Corrupt;
    };

    let Ok(json_str) = std::str::from_utf8(&plaintext) else {
        return ReadSdkResult::Corrupt;
    };
    let Ok(credentials) = serde_json::from_str::<SdkCredentials>(json_str) else {
        return ReadSdkResult::Corrupt;
    };

    let crypto = CryptoParams {
        salt,
        wrapped_key: wrapped_bytes,
        data_key,
    };

    ReadSdkResult::Parsed {
        credentials: Box::new(credentials),
        crypto: Box::new(crypto),
        plaintext,
    }
}

pub(crate) struct RewriteFields<'a> {
    pub opaque_at: &'a str,
    pub opaque_rt: Option<&'a str>,
    pub expires: Option<u64>,
    pub user_id: Option<&'a str>,
    pub granted_scopes: Option<&'a [String]>,
}

pub(crate) fn rewrite_sdk_credentials(
    leveldb_path: &Path,
    original_plaintext: &[u8],
    fields: &RewriteFields<'_>,
    crypto: &CryptoParams,
) -> Option<()> {
    let mut json: serde_json::Value = serde_json::from_slice(original_plaintext).ok()?;

    if let Some(at) = json.get_mut("accessToken").and_then(|v| v.as_object_mut()) {
        at.insert(
            "token".to_string(),
            serde_json::Value::String(fields.opaque_at.to_string()),
        );
        if let Some(expires) = fields.expires {
            at.insert("expires".to_string(), serde_json::json!(expires));
        }
        if let Some(user_id) = fields.user_id {
            at.insert(
                "userId".to_string(),
                serde_json::Value::String(user_id.to_string()),
            );
        }
        if let Some(scopes) = fields.granted_scopes {
            at.insert("grantedScopes".to_string(), serde_json::json!(scopes));
        }
    }
    if let Some(rt) = fields.opaque_rt {
        json.as_object_mut()?.insert(
            "refreshToken".to_string(),
            serde_json::Value::String(rt.to_string()),
        );
    }

    let new_plaintext = serde_json::to_vec(&json).ok()?;

    let mut new_counter = [0u8; COUNTER_LEN];
    getrandom::fill(&mut new_counter).ok()?;

    let ciphertext = encrypt_aes_ctr(&crypto.data_key, &new_counter, &new_plaintext)?;

    let OpenResult::Ok(db) = open_leveldb(leveldb_path) else {
        return None;
    };
    let mut db = *db;
    write_ls_keys(
        &mut db,
        &[
            ("AuthDB/tidalData", &ciphertext),
            ("AuthDB/tidalCounter", &new_counter),
        ],
    )?;

    Some(())
}

fn try_into_array<const N: usize>(slice: &[u8]) -> Option<[u8; N]> {
    slice.try_into().ok()
}

enum OpenResult {
    Missing,
    Ok(Box<rusty_leveldb::DB>),
    Error,
}

/// Chromium writes with Snappy (compressor id=1). Default CompressorList includes both
/// NoneCompressor (0) and SnappyCompressor (1), so reads work. compressor=1 for writes.
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
