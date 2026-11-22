// FRAGILE: depends on Chromium LevelDB localStorage internals + TIDAL SDK crypto scheme.
// Revalidate on Chromium/CEF upgrades (LevelDB key/value format) and TIDAL SDK updates
// (PBKDF2 params, AES-KW, AES-CTR counter_bits, localStorage key names).
//
// Fail-closed: open_credentials() returns Missing, Unreadable, or every credential blob of the
// origin; CredentialBlob::decrypt() returns None on any crypto or schema failure. Never panics.
//
// Everything that opens the LevelDB runs before CEF init only: Chromium locks that directory once
// it starts.

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
/// The localStorage prefix every credential blob of TIDAL's SDK sits under, whatever its id.
const CREDENTIALS_PREFIX: &str = "AuthDB/";
/// The four values one blob is stored as, `AuthDB/<id><suffix>`, in the order `CredentialBlob`
/// keeps them.
const BLOB_SUFFIXES: [&str; 4] = ["Salt", "Counter", "Key", "Data"];
/// Chromium's two encodings of a localStorage key or value, named by its first byte.
const UTF16_LE: u8 = 0x00;
const LATIN1: u8 = 0x01;

/// Removes every SDK credential key of TIDAL's origin, each id and suffix included, before the
/// document it runs in starts the SDK. The origin is checked first: anywhere else `localStorage`
/// may throw, and this runs nowhere else.
pub(crate) fn credentials_purge_script() -> String {
    let origin = serde_json::Value::from(ORIGIN);
    let prefix = serde_json::Value::from(CREDENTIALS_PREFIX);
    format!(
        "if(location.origin==={origin}){{Object.keys(localStorage).forEach(function(k){{if(k.startsWith({prefix}))localStorage.removeItem(k)}})}}"
    )
}

/// The part of a Chrome localStorage LevelDB key that names TIDAL's origin: `_` + origin + `\x00`.
fn origin_key_prefix() -> Vec<u8> {
    let mut key = Vec::with_capacity(1 + ORIGIN.len() + 1);
    key.push(b'_');
    key.extend_from_slice(ORIGIN.as_bytes());
    key.push(0x00);
    key
}

/// Build a Chrome localStorage LevelDB key.
/// Format: `_` + origin + `\x00` + encoding_prefix + key_name
fn leveldb_key(ls_key: &str) -> Vec<u8> {
    let mut key = origin_key_prefix();
    key.push(LATIN1);
    key.extend_from_slice(ls_key.as_bytes());
    key
}

/// The start of every credential key of the origin, in each encoding Chromium may store a key in.
fn credential_key_prefixes() -> [Vec<u8>; 2] {
    let mut utf16 = origin_key_prefix();
    utf16.push(UTF16_LE);
    for unit in CREDENTIALS_PREFIX.encode_utf16() {
        utf16.extend_from_slice(&unit.to_le_bytes());
    }
    [leveldb_key(CREDENTIALS_PREFIX), utf16]
}

/// A localStorage key name as the page's JS sees it. Chromium stores a name as Latin-1 when every
/// character fits, as UTF-16-LE otherwise.
fn decode_ls_name(raw: &[u8]) -> Option<String> {
    match raw.split_first()? {
        (&LATIN1, name) => Some(name.iter().copied().map(char::from).collect()),
        (&UTF16_LE, name) if name.len().is_multiple_of(2) => {
            let units: Vec<u16> = name
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| u16::from_le_bytes(*pair))
                .collect();
            Some(String::from_utf16_lossy(&units))
        }
        _ => None,
    }
}

/// Decode a Chrome localStorage value.
/// First byte = encoding (0x01 = Latin-1 bytes, 0x00 = UTF-16-LE).
/// Returns raw bytes (the char codes).
fn decode_ls_value(raw: &[u8]) -> Option<Vec<u8>> {
    if raw.is_empty() {
        return None;
    }
    match raw[0] {
        LATIN1 => Some(raw[1..].to_vec()),
        UTF16_LE => {
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
fn derive_wrapping_key(password: &[u8], salt: &[u8; SALT_LEN]) -> [u8; PBKDF2_KEY_LEN] {
    let mut key = [0u8; PBKDF2_KEY_LEN];
    pbkdf2::pbkdf2_hmac::<sha2::Sha256>(password, salt, PBKDF2_ITERATIONS, &mut key);
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
    /// The account the SDK wrote this blob for, as a decimal string.
    pub user_id: Option<String>,
}

/// One credential blob of TIDAL's SDK, `AuthDB/<id>{Salt,Counter,Key,Data}`, as Local Storage
/// holds it. Ids are read off the key names, never assumed.
pub(crate) struct CredentialBlob {
    /// The id the SDK stored it under, which is also the password it encrypted it with.
    pub(crate) id: String,
    /// Every LevelDB key found under the id, a name no suffix ends included.
    keys: Vec<Vec<u8>>,
    /// The values in `BLOB_SUFFIXES` order, `None` when absent or in neither of Chromium's encodings.
    parts: [Option<Vec<u8>>; 4],
}

impl CredentialBlob {
    /// Decrypts it as the SDK does, with its id as the PBKDF2 password. `None` when a part is
    /// missing or malformed, the key does not unwrap, or the JSON is not the SDK's. Runs the
    /// 100k-iteration PBKDF2.
    pub(crate) fn decrypt(&self) -> Option<SdkCredentials> {
        let [salt, counter, wrapped_key, data] = &self.parts;
        let salt = <[u8; SALT_LEN]>::try_from(salt.as_deref()?).ok()?;
        let counter = <[u8; COUNTER_LEN]>::try_from(counter.as_deref()?).ok()?;
        let wrapping_key = derive_wrapping_key(self.id.as_bytes(), &salt);
        let data_key = unwrap_data_key(&wrapping_key, wrapped_key.as_deref()?)?;
        let plaintext = decrypt_aes_ctr(&data_key, &counter, data.as_deref()?)?;
        serde_json::from_slice(&plaintext).ok()
    }
}

/// Sorts the credential entries of the origin into blobs by the id their names carry. A name no
/// suffix ends, or one that does not decode, is a blob of its own that never decrypts.
fn group_blobs(entries: Vec<(Vec<u8>, Vec<u8>)>) -> Vec<CredentialBlob> {
    let name_at = origin_key_prefix().len();
    let mut blobs = std::collections::BTreeMap::<String, CredentialBlob>::new();
    for (key, value) in entries {
        let name = key
            .get(name_at..)
            .and_then(decode_ls_name)
            .unwrap_or_default();
        let rest = name.strip_prefix(CREDENTIALS_PREFIX).unwrap_or(&name);
        let (id, part) = match BLOB_SUFFIXES
            .iter()
            .position(|suffix| rest.ends_with(suffix))
        {
            Some(part) => (&rest[..rest.len() - BLOB_SUFFIXES[part].len()], Some(part)),
            None => (rest, None),
        };
        let blob = blobs
            .entry(id.to_owned())
            .or_insert_with(|| CredentialBlob {
                id: id.to_owned(),
                keys: Vec::new(),
                parts: Default::default(),
            });
        if let Some(part) = part {
            blob.parts[part] = decode_ls_value(&value);
        }
        blob.keys.push(key);
    }
    blobs.into_values().collect()
}

/// Every entry of the origin under the credential prefix, through the iterator (see
/// `read_ls_key`). `None` when the tables do not read, a damaged one panicking instead of erroring.
fn credential_entries(db: &mut rusty_leveldb::DB) -> Option<Vec<(Vec<u8>, Vec<u8>)>> {
    use rusty_leveldb::LdbIterator;

    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut entries = Vec::new();
        for prefix in credential_key_prefixes() {
            let mut iter = db.new_iter().ok()?;
            iter.seek(&prefix);
            while iter.valid() {
                let Some((key, value)) = iter.current() else {
                    break;
                };
                if !key.starts_with(&prefix) {
                    break;
                }
                entries.push((key.to_vec(), value.to_vec()));
                if !iter.advance() {
                    break;
                }
            }
        }
        Some(entries)
    }))
    .ok()
    .flatten()
}

/// Local Storage with every SDK credential blob of TIDAL's origin.
pub(crate) struct CredentialStore {
    db: rusty_leveldb::DB,
    blobs: Vec<CredentialBlob>,
}

pub(crate) enum OpenedCredentials {
    /// No Local Storage: the browser never started on this profile.
    Missing,
    Opened(Box<CredentialStore>),
    /// It would not open (another process holds it, most often) or its tables would not read:
    /// nothing may be concluded from it, and nothing is written to it.
    Unreadable,
}

pub(crate) fn open_credentials(leveldb_path: &Path) -> OpenedCredentials {
    let mut db = match open_leveldb(leveldb_path, OpenMode::Existing) {
        OpenResult::Missing => return OpenedCredentials::Missing,
        OpenResult::Ok(db) => *db,
        OpenResult::Error => return OpenedCredentials::Unreadable,
    };
    match credential_entries(&mut db) {
        Some(entries) => OpenedCredentials::Opened(Box::new(CredentialStore {
            db,
            blobs: group_blobs(entries),
        })),
        None => OpenedCredentials::Unreadable,
    }
}

/// What a purge of credential blobs did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Purge {
    /// No blob was picked: nothing was written.
    Nothing,
    /// That many keys went, in one batch.
    Deleted(usize),
    /// The batch did not land.
    Failed,
}

impl CredentialStore {
    pub(crate) fn blobs(&self) -> &[CredentialBlob] {
        &self.blobs
    }

    /// Deletes every key of the blobs `doomed` picks, in one batch, then closes the store.
    pub(crate) fn purge(mut self, doomed: impl Fn(&CredentialBlob) -> bool) -> Purge {
        let mut batch = rusty_leveldb::WriteBatch::default();
        let mut deleted = 0;
        for key in self
            .blobs
            .iter()
            .filter(|blob| doomed(blob))
            .flat_map(|blob| &blob.keys)
        {
            batch.delete(key);
            deleted += 1;
        }
        if deleted == 0 {
            return Purge::Nothing;
        }
        match self.db.write(batch, true) {
            Ok(()) => Purge::Deleted(deleted),
            Err(_) => Purge::Failed,
        }
    }
}

enum OpenResult {
    Missing,
    Ok(Box<rusty_leveldb::DB>),
    Error,
}

enum OpenMode {
    Existing,
    /// The worker-cache epoch write can run on a fresh profile.
    OrCreate,
}

/// Chromium writes with Snappy (compressor id=1). Default CompressorList includes both
/// NoneCompressor (0) and SnappyCompressor (1); reads work. compressor=1 for writes.
fn open_leveldb(path: &Path, mode: OpenMode) -> OpenResult {
    if matches!(mode, OpenMode::Existing) && !path.exists() {
        return OpenResult::Missing;
    }
    // On Windows both sides lock with `LockFileEx` and the open itself refuses.
    #[cfg(unix)]
    if matches!(probe_lock(path), LockProbe::Held) {
        return OpenResult::Error;
    }
    let opts = rusty_leveldb::Options {
        create_if_missing: matches!(mode, OpenMode::OrCreate),
        compressor: 1, // Snappy, same as Chromium
        ..rusty_leveldb::Options::default()
    };
    match rusty_leveldb::DB::open(path, opts) {
        Ok(db) => OpenResult::Ok(Box::new(db)),
        Err(_) => OpenResult::Error,
    }
}

#[cfg(unix)]
enum LockProbe {
    Free,
    Held,
}

/// Chromium locks `LOCK` with `fcntl`, which rusty-leveldb's `flock` does not see on Linux; the
/// probe asks with Chromium's own call. Anything it cannot answer counts as held. Before CEF
/// init only: `F_GETLK` misses this process's own locks, and closing the file drops them.
#[cfg(unix)]
fn probe_lock(leveldb: &Path) -> LockProbe {
    use std::io::ErrorKind;
    use std::os::fd::AsRawFd;

    let file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(leveldb.join("LOCK"))
    {
        Ok(file) => file,
        Err(e) if e.kind() == ErrorKind::NotFound => return LockProbe::Free,
        Err(_) => return LockProbe::Held,
    };
    // SAFETY: `flock` is a C struct; the fields left alone are the ones read as zero (whole file).
    let mut lock: libc::flock = unsafe { std::mem::zeroed() };
    lock.l_type = libc::F_WRLCK as _;
    lock.l_whence = libc::SEEK_SET as _;
    // SAFETY: the descriptor belongs to `file`, which outlives the call, and `lock` is ours.
    let rc = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETLK, &mut lock) };
    if rc == -1 || lock.l_type != libc::F_UNLCK as libc::c_short {
        LockProbe::Held
    } else {
        LockProbe::Free
    }
}

enum ReadKeyResult {
    Missing,
    Ok(Vec<u8>),
    Invalid,
    /// The tables could not be read; nothing may be concluded from them.
    Unreadable,
}

enum Lookup {
    Found(Vec<u8>),
    Absent,
    Failed,
}

/// Looks the key up through the iterator, because `DB::get` stops at the first tombstone it meets
/// even when it belongs to another key (rusty-leveldb 4.0.2, `version.rs:95`), missing live keys
/// on Chromium's multi-table stores. A damaged table panics there instead of erroring.
fn read_ls_key(db: &mut rusty_leveldb::DB, ls_key: &str) -> ReadKeyResult {
    use rusty_leveldb::LdbIterator;

    let key = leveldb_key(ls_key);
    let lookup = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let Ok(mut iter) = db.new_iter() else {
            return Lookup::Failed;
        };
        iter.seek(&key);
        match iter.current() {
            Some((found, raw)) if iter.valid() && found.as_ref() == key.as_slice() => {
                Lookup::Found(raw.to_vec())
            }
            _ => Lookup::Absent,
        }
    }))
    .unwrap_or(Lookup::Failed);
    match lookup {
        Lookup::Found(raw) => match decode_ls_value(&raw) {
            Some(bytes) => ReadKeyResult::Ok(bytes),
            None => ReadKeyResult::Invalid,
        },
        Lookup::Absent => ReadKeyResult::Missing,
        Lookup::Failed => ReadKeyResult::Unreadable,
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

/// One localStorage value of TIDAL's origin, read with the LevelDB opened here.
pub(crate) enum LsValue {
    Missing,
    Value(Vec<u8>),
    /// Present but not in either of Chromium's value encodings.
    Corrupt,
    /// The LevelDB would not open or its tables would not read.
    Unreadable,
}

pub(crate) fn read_ls_value(leveldb_path: &Path, ls_key: &str) -> LsValue {
    let mut db = match open_leveldb(leveldb_path, OpenMode::Existing) {
        OpenResult::Missing => return LsValue::Missing,
        OpenResult::Ok(db) => *db,
        OpenResult::Error => return LsValue::Unreadable,
    };
    match read_ls_key(&mut db, ls_key) {
        ReadKeyResult::Ok(bytes) => LsValue::Value(bytes),
        ReadKeyResult::Missing => LsValue::Missing,
        ReadKeyResult::Invalid => LsValue::Corrupt,
        ReadKeyResult::Unreadable => LsValue::Unreadable,
    }
}

/// Writes localStorage values in one batch, creating the LevelDB if absent.
pub(crate) fn write_ls_values(leveldb_path: &Path, entries: &[(&str, &[u8])]) -> Option<()> {
    let OpenResult::Ok(mut db) = open_leveldb(leveldb_path, OpenMode::OrCreate) else {
        return None;
    };
    write_ls_keys(&mut db, entries)
}
