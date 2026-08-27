use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Serialize, Deserialize)]
pub(crate) struct StoredTokenState {
    pub current: TokenGeneration,
    pub previous: Option<TokenGeneration>,
    pub previous_valid_until: Option<u64>,
}

#[derive(Serialize, Deserialize, Clone)]
pub(crate) struct TokenGeneration {
    pub access_token: String,
    pub refresh_token: String,
    pub opaque_at: String,
    pub opaque_rt: String,
    pub version: u64,
    pub access_expires: u64,
    pub user_id: Option<String>,
    pub granted_scopes: Vec<String>,
    pub client_id: String,
}

impl TokenGeneration {
    /// True when this generation can still re-establish a session on a later
    /// launch, which is the only reason to keep one on disk.
    ///
    /// The refresh token is the sole part that outlives the access token's
    /// expiry. A generation without one is scoped to the running process: it
    /// serves requests now and is worth nothing stored. Two exchanges arrive
    /// that way, both legitimate - an app-level `client_credentials` token, and
    /// any response that reuses a refresh token we no longer hold.
    pub(crate) fn is_durable(&self) -> bool {
        !self.refresh_token.is_empty()
    }

    /// The access-token expiry in the unit TIDAL's own SDK reads.
    ///
    /// `access_expires` is a seconds epoch everywhere on this side, but every
    /// expiry handed to the web SDK is a milliseconds one: its OAuth parser
    /// stores `Date.now() + expires_in * 1000`, and its freshness check compares
    /// against `Date.now() + 60000`. Seconds are smaller by a factor of a
    /// thousand, so a token passed in them reads as long expired however much
    /// life it has left. Both paths that reach the SDK come through here, which
    /// is the point: they used to disagree.
    pub(crate) fn access_expires_ms(&self) -> u64 {
        self.access_expires.saturating_mul(1000)
    }
}

#[derive(Debug)]
#[allow(dead_code)] // not every variant is constructed on every platform
pub(crate) enum StoreError {
    /// Backend not available (no Keychain, DPAPI failure, etc.)
    Unavailable,
    /// Backend accessible but operation failed (permission, I/O, lock). Carries the
    /// platform's own message: the variant alone cannot tell a refused Keychain
    /// prompt from a full disk, and that distinction is the reason to report it.
    Backend(String),
    /// Data present but not valid JSON / wrong schema
    Corrupt,
}

pub(crate) fn save(data_dir: &Path, state: &StoredTokenState) -> Result<(), StoreError> {
    let json = serde_json::to_vec(state).map_err(|_| StoreError::Corrupt)?;
    save_platform(data_dir, &json)
}

pub(crate) fn load(data_dir: &Path) -> Result<Option<StoredTokenState>, StoreError> {
    let Some(json) = load_platform(data_dir)? else {
        return Ok(None);
    };
    serde_json::from_slice(&json)
        .map(Some)
        .map_err(|_| StoreError::Corrupt)
}

pub(crate) fn delete(data_dir: &Path) -> Result<(), StoreError> {
    delete_platform(data_dir)
}

// --- Windows: DPAPI ---

#[cfg(target_os = "windows")]
fn save_platform(data_dir: &Path, plaintext: &[u8]) -> Result<(), StoreError> {
    use std::io::Write;
    use windows::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData,
    };

    let input = CRYPT_INTEGER_BLOB {
        cbData: plaintext.len() as u32,
        pbData: plaintext.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB::default();

    unsafe {
        CryptProtectData(
            &input,
            None,
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    }
    .map_err(|e| StoreError::Backend(e.to_string()))?;

    let encrypted =
        unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize) }.to_vec();
    unsafe { windows_sys::Win32::Foundation::LocalFree(output.pbData.cast()) };

    let path = data_dir.join("auth_tokens.dpapi");
    // Best-effort durability: sync_all flushes file content, persist does an atomic
    // rename. Directory fsync is not done - a power loss right after persist could
    // lose the entry, which is acceptable (triggers re-login, not corruption).
    let mut f = tempfile::NamedTempFile::new_in(data_dir)
        .map_err(|e| StoreError::Backend(e.to_string()))?;
    f.write_all(&encrypted)
        .map_err(|e| StoreError::Backend(e.to_string()))?;
    f.as_file()
        .sync_all()
        .map_err(|e| StoreError::Backend(e.to_string()))?;
    f.persist(&path)
        .map_err(|e| StoreError::Backend(e.to_string()))?;
    Ok(())
}

#[cfg(target_os = "windows")]
fn load_platform(data_dir: &Path) -> Result<Option<Vec<u8>>, StoreError> {
    use windows::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptUnprotectData,
    };

    let path = data_dir.join("auth_tokens.dpapi");
    let encrypted = match std::fs::read(&path) {
        Ok(v) => v,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(StoreError::Backend(e.to_string())),
    };

    let input = CRYPT_INTEGER_BLOB {
        cbData: encrypted.len() as u32,
        pbData: encrypted.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB::default();

    unsafe {
        CryptUnprotectData(
            &input,
            None,
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    }
    .map_err(|_| StoreError::Corrupt)?;

    let decrypted =
        unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize) }.to_vec();
    unsafe { windows_sys::Win32::Foundation::LocalFree(output.pbData.cast()) };

    Ok(Some(decrypted))
}

#[cfg(target_os = "windows")]
fn delete_platform(data_dir: &Path) -> Result<(), StoreError> {
    match std::fs::remove_file(data_dir.join("auth_tokens.dpapi")) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(StoreError::Backend(e.to_string())),
    }
}

// --- macOS: Keychain ---

#[cfg(target_os = "macos")]
fn save_platform(_data_dir: &Path, plaintext: &[u8]) -> Result<(), StoreError> {
    security_framework::passwords::set_generic_password("com.tidaluna", "auth_state", plaintext)
        .map_err(|e| StoreError::Backend(e.to_string()))
}

#[cfg(target_os = "macos")]
fn load_platform(_data_dir: &Path) -> Result<Option<Vec<u8>>, StoreError> {
    match security_framework::passwords::get_generic_password("com.tidaluna", "auth_state") {
        Ok(v) => Ok(Some(v)),
        Err(e) if e.code() == -25300 => Ok(None), // errSecItemNotFound
        Err(e) => Err(StoreError::Backend(e.to_string())),
    }
}

#[cfg(target_os = "macos")]
fn delete_platform(_data_dir: &Path) -> Result<(), StoreError> {
    match security_framework::passwords::delete_generic_password("com.tidaluna", "auth_state") {
        Ok(()) => Ok(()),
        Err(e) if e.code() == -25300 => Ok(()), // errSecItemNotFound
        Err(e) => Err(StoreError::Backend(e.to_string())),
    }
}

// --- Linux: 0600 file ---
//
// No Secret Service / keyring: it only ever protected against local OS-level
// threats (other users, offline disk access), which are out of scope here, and
// it forces a keyring-unlock password prompt on every fresh session. The token
// is kept away from plugins by path containment, not at-rest encryption: JS
// plugins have no filesystem access, and native (Bun) plugins get an fs facade
// scoped to cache_data_dir()/native/<plugin>/; they cannot reach this file in
// the data-dir root. A 0600 file in the data dir therefore satisfies the
// in-scope boundary with no prompt.

#[cfg(target_os = "linux")]
fn save_platform(data_dir: &Path, plaintext: &[u8]) -> Result<(), StoreError> {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    let path = data_dir.join("auth_tokens.json");
    let mut f = tempfile::NamedTempFile::new_in(data_dir)
        .map_err(|e| StoreError::Backend(e.to_string()))?;
    f.as_file()
        .set_permissions(std::fs::Permissions::from_mode(0o600))
        .map_err(|e| StoreError::Backend(e.to_string()))?;
    f.write_all(plaintext)
        .map_err(|e| StoreError::Backend(e.to_string()))?;
    f.as_file()
        .sync_all()
        .map_err(|e| StoreError::Backend(e.to_string()))?;
    f.persist(&path)
        .map_err(|e| StoreError::Backend(e.to_string()))?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn load_platform(data_dir: &Path) -> Result<Option<Vec<u8>>, StoreError> {
    match std::fs::read(data_dir.join("auth_tokens.json")) {
        Ok(v) => Ok(Some(v)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(StoreError::Backend(e.to_string())),
    }
}

#[cfg(target_os = "linux")]
fn delete_platform(data_dir: &Path) -> Result<(), StoreError> {
    match std::fs::remove_file(data_dir.join("auth_tokens.json")) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(StoreError::Backend(e.to_string())),
    }
}

#[cfg(test)]
#[path = "../../tests/unit/platform/secure_store.rs"]
mod tests;
