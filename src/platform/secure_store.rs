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

pub(crate) enum StoreError {
    /// Backend not available (no Secret Service on Linux, no Keychain, etc.)
    Unavailable,
    /// Backend accessible but operation failed (permission, I/O, lock)
    Backend,
    /// Data present but not valid JSON / wrong schema
    Corrupt,
}

pub(crate) fn save(data_dir: &Path, state: &StoredTokenState) -> Result<(), StoreError> {
    let json = serde_json::to_vec(state).map_err(|_| StoreError::Backend)?;
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

    let mut input = CRYPT_INTEGER_BLOB {
        cbData: plaintext.len() as u32,
        pbData: plaintext.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB::default();

    unsafe {
        CryptProtectData(
            &mut input,
            None,
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    }
    .map_err(|_| StoreError::Backend)?;

    let encrypted =
        unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize) }.to_vec();
    unsafe { windows_sys::Win32::Foundation::LocalFree(output.pbData.cast()) };

    let path = data_dir.join("auth_tokens.dpapi");
    // Best-effort durability: sync_all flushes file content, persist does an atomic
    // rename. Directory fsync is not done — a power loss right after persist could
    // lose the entry, which is acceptable (triggers re-login, not corruption).
    let mut f = tempfile::NamedTempFile::new_in(data_dir).map_err(|_| StoreError::Backend)?;
    f.write_all(&encrypted).map_err(|_| StoreError::Backend)?;
    f.as_file().sync_all().map_err(|_| StoreError::Backend)?;
    f.persist(&path).map_err(|_| StoreError::Backend)?;
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
        Err(_) => return Err(StoreError::Backend),
    };

    let mut input = CRYPT_INTEGER_BLOB {
        cbData: encrypted.len() as u32,
        pbData: encrypted.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB::default();

    unsafe {
        CryptUnprotectData(
            &mut input,
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
        Err(_) => Err(StoreError::Backend),
    }
}

// --- macOS: Keychain ---

#[cfg(target_os = "macos")]
fn save_platform(_data_dir: &Path, plaintext: &[u8]) -> Result<(), StoreError> {
    security_framework::passwords::set_generic_password("com.tidaluna", "auth_state", plaintext)
        .map_err(|_| StoreError::Backend)
}

#[cfg(target_os = "macos")]
fn load_platform(_data_dir: &Path) -> Result<Option<Vec<u8>>, StoreError> {
    match security_framework::passwords::get_generic_password("com.tidaluna", "auth_state") {
        Ok(v) => Ok(Some(v)),
        Err(e) if e.code() == -25300 => Ok(None), // errSecItemNotFound
        Err(_) => Err(StoreError::Backend),
    }
}

#[cfg(target_os = "macos")]
fn delete_platform(_data_dir: &Path) -> Result<(), StoreError> {
    match security_framework::passwords::delete_generic_password("com.tidaluna", "auth_state") {
        Ok(()) => Ok(()),
        Err(e) if e.code() == -25300 => Ok(()), // errSecItemNotFound
        Err(_) => Err(StoreError::Backend),
    }
}

// --- Linux: keyring ---

#[cfg(target_os = "linux")]
fn save_platform(_data_dir: &Path, plaintext: &[u8]) -> Result<(), StoreError> {
    let entry =
        keyring::Entry::new("com.tidaluna", "auth_state").map_err(|_| StoreError::Unavailable)?;
    entry
        .set_secret(plaintext)
        .map_err(|e| match classify_keyring_error(&e) {
            KeyringErrorKind::NoAccess => StoreError::Unavailable,
            KeyringErrorKind::Other => StoreError::Backend,
        })
}

#[cfg(target_os = "linux")]
fn load_platform(_data_dir: &Path) -> Result<Option<Vec<u8>>, StoreError> {
    let entry =
        keyring::Entry::new("com.tidaluna", "auth_state").map_err(|_| StoreError::Unavailable)?;
    match entry.get_secret() {
        Ok(v) => Ok(Some(v)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(match classify_keyring_error(&e) {
            KeyringErrorKind::NoAccess => StoreError::Unavailable,
            KeyringErrorKind::Other => StoreError::Backend,
        }),
    }
}

#[cfg(target_os = "linux")]
fn delete_platform(_data_dir: &Path) -> Result<(), StoreError> {
    let entry =
        keyring::Entry::new("com.tidaluna", "auth_state").map_err(|_| StoreError::Unavailable)?;
    match entry.delete_credential() {
        Ok(()) => Ok(()),
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(match classify_keyring_error(&e) {
            KeyringErrorKind::NoAccess => StoreError::Unavailable,
            KeyringErrorKind::Other => StoreError::Backend,
        }),
    }
}

#[cfg(target_os = "linux")]
enum KeyringErrorKind {
    NoAccess,
    Other,
}

#[cfg(target_os = "linux")]
fn classify_keyring_error(e: &keyring::Error) -> KeyringErrorKind {
    match e {
        keyring::Error::NoStorageAccess(_) => KeyringErrorKind::NoAccess,
        _ => KeyringErrorKind::Other,
    }
}
