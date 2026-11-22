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
    #[serde(flatten)]
    pub client: ClientBinding,
}

/// The client a generation's refresh token is sent with, and whether TIDAL has accepted it for
/// that token. On disk a `client_id` and a `client_proven` flag; a store written before the flag
/// reads back unproven.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "ClientBindingOnDisk", into = "ClientBindingOnDisk")]
pub(crate) enum ClientBinding {
    /// Stored but never seen accepted: a store older than the flag, or a sign-in whose client
    /// nothing observed.
    Unproven(String),
    /// TIDAL answered a request sent with it, for this refresh token.
    Proven(String),
}

#[derive(Clone, Serialize, Deserialize)]
struct ClientBindingOnDisk {
    client_id: String,
    #[serde(default)]
    client_proven: bool,
}

impl From<ClientBindingOnDisk> for ClientBinding {
    fn from(disk: ClientBindingOnDisk) -> Self {
        if disk.client_proven {
            Self::Proven(disk.client_id)
        } else {
            Self::Unproven(disk.client_id)
        }
    }
}

impl From<ClientBinding> for ClientBindingOnDisk {
    fn from(binding: ClientBinding) -> Self {
        let client_proven = matches!(binding, ClientBinding::Proven(_));
        let (ClientBinding::Unproven(client_id) | ClientBinding::Proven(client_id)) = binding;
        Self {
            client_id,
            client_proven,
        }
    }
}

impl ClientBinding {
    pub(crate) fn id(&self) -> &str {
        let (Self::Unproven(id) | Self::Proven(id)) = self;
        id
    }

    /// The client a refresh the page sent leaves with. A proven client is pinned; an unproven one
    /// lets the page's own through, the server then judging it, except on `update_client`, the
    /// SDK's move to the page's client, which is only ever sent with the stored one. `None` sends
    /// no client at all.
    pub(crate) fn for_page_refresh<'a>(
        &'a self,
        asked: Option<&'a str>,
        update_client: bool,
    ) -> Option<&'a str> {
        match self {
            Self::Proven(id) if !id.is_empty() => Some(id),
            Self::Unproven(id) if update_client && !id.is_empty() => Some(id),
            Self::Proven(_) | Self::Unproven(_) => asked,
        }
    }

    /// A sign-in's new refresh token is bound to the client its exchange was seen sent with;
    /// unseen, the prior client stays, unproven for a token it never served.
    pub(crate) fn after_login(prior: Option<&Self>, sent: Option<&str>) -> Self {
        match sent.filter(|id| !id.is_empty()) {
            Some(id) => Self::Proven(id.to_owned()),
            None => Self::Unproven(prior.map(|p| p.id().to_owned()).unwrap_or_default()),
        }
    }

    /// A refresh TIDAL answered proves the client it was sent with, for the token it refreshed;
    /// sent without one, the prior binding stays as it was.
    pub(crate) fn after_refresh(prior: Option<&Self>, sent: Option<&str>) -> Self {
        match sent.filter(|id| !id.is_empty()) {
            Some(id) => Self::Proven(id.to_owned()),
            None => prior
                .cloned()
                .unwrap_or_else(|| Self::Unproven(String::new())),
        }
    }
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
}

#[derive(Debug)]
pub(crate) enum StoreError {
    /// Backend accessible but operation failed (permission, I/O, lock). Carries the
    /// platform's own message: the variant alone cannot tell a refused Keychain
    /// prompt from a full disk, and that distinction is the reason to report it.
    Backend(String),
    /// Data present but not valid JSON / wrong schema
    Corrupt,
}

pub(crate) fn load(data_dir: &Path) -> Result<Option<StoredTokenState>, StoreError> {
    let Some(json) = load_platform(data_dir)? else {
        return Ok(None);
    };
    serde_json::from_slice(&json)
        .map(Some)
        .map_err(|_| StoreError::Corrupt)
}

/// What the writer thread replays onto the backing store, in the order it was queued.
enum StoreOp {
    Save(Vec<u8>),
    Delete,
    /// A rendezvous queued behind the pending writes: whoever sent it learns they have all
    /// reached the disk. Same shape as `DbActor::flush`, for the same reason.
    Flush(std::sync::mpsc::SyncSender<()>),
}

static STORE_WRITER: std::sync::OnceLock<std::sync::mpsc::SyncSender<StoreOp>> =
    std::sync::OnceLock::new();

/// The one thread allowed to touch the credential store, started on first use.
///
/// `data_dir` is fixed for the process: capturing the first caller's copy is safe.
fn store_writer(data_dir: &Path) -> &'static std::sync::mpsc::SyncSender<StoreOp> {
    STORE_WRITER.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::sync_channel::<StoreOp>(16);
        let dir = data_dir.to_owned();
        std::thread::Builder::new()
            .name("secure-store".into())
            .spawn(move || {
                while let Ok(op) = rx.recv() {
                    let outcome = match op {
                        StoreOp::Save(json) => save_platform(&dir, &json),
                        StoreOp::Delete => delete_platform(&dir),
                        StoreOp::Flush(done) => {
                            let _ = done.send(());
                            continue;
                        }
                    };
                    if let Err(e) = outcome {
                        // Ungated: nothing awaits a queued write, and this is the only place
                        // its failure can surface.
                        crate::verr!("[AUTH]   Credential store write failed: {e:?}");
                    }
                }
            })
            .expect("failed to spawn secure-store thread");
        tx
    })
}

/// Queue a credential write rather than performing it here.
///
/// Three writers race for this store: CEF's IO thread through the token filter, and two tokio
/// tasks. What used to order them was the `AppState` mutex held across the write itself, making
/// a process-wide lock span an fsync. Callers now mint their generation and queue it under that
/// same lock, and one thread replays mint order onto the disk with the lock long released.
/// [`delete_queued`] shares the channel on purpose: a logout that erased the store while an
/// older save sat queued would put the credential back after the user left.
pub(crate) fn save_queued(data_dir: &Path, state: &StoredTokenState) {
    let json = match serde_json::to_vec(state) {
        Ok(json) => json,
        Err(e) => {
            crate::verr!("[AUTH]   Credential state could not be encoded, not saved: {e}");
            return;
        }
    };
    if store_writer(data_dir).send(StoreOp::Save(json)).is_err() {
        crate::verr!("[AUTH]   Credential save dropped: the secure-store thread is dead");
    }
}

/// Queue a credential erase. Ordered against [`save_queued`]; see its note.
pub(crate) fn delete_queued(data_dir: &Path) {
    if store_writer(data_dir).send(StoreOp::Delete).is_err() {
        crate::verr!("[AUTH]   Credential erase dropped: the secure-store thread is dead");
    }
}

/// Wait until every write queued so far has reached the disk.
///
/// The writes are queued, the thread performing them is detached, and process exit waits for
/// neither: a login or logout followed by a quick quit loses its write. A lost save costs a
/// re-login; a lost erase is worse, the next launch restoring the session the user just left.
/// Does nothing if no write was ever queued, the writer thread starting on first use.
pub(crate) fn flush() {
    let Some(tx) = STORE_WRITER.get() else {
        return;
    };
    let (done_tx, done_rx) = std::sync::mpsc::sync_channel::<()>(0);
    if tx.send(StoreOp::Flush(done_tx)).is_ok() {
        let _ = done_rx.recv();
    }
}

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
    // sync_all flushes the content, durable_fs::persist the rename: a save lost to a power cut
    // would put back an older generation.
    let mut f = tempfile::NamedTempFile::new_in(data_dir)
        .map_err(|e| StoreError::Backend(e.to_string()))?;
    f.write_all(&encrypted)
        .map_err(|e| StoreError::Backend(e.to_string()))?;
    f.as_file()
        .sync_all()
        .map_err(|e| StoreError::Backend(e.to_string()))?;
    crate::platform::durable_fs::persist(f, &path)
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
        // What `durable_fs::tombstone` leaves of an erase whose unlink a power cut took back.
        Ok(v) if v.is_empty() => return Ok(None),
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
    crate::platform::durable_fs::remove_file(&data_dir.join("auth_tokens.dpapi"))
        .map_err(|e| StoreError::Backend(e.to_string()))
}

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
    crate::platform::durable_fs::persist(f, &path)
        .map_err(|e| StoreError::Backend(e.to_string()))?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn load_platform(data_dir: &Path) -> Result<Option<Vec<u8>>, StoreError> {
    match std::fs::read(data_dir.join("auth_tokens.json")) {
        // What `durable_fs::tombstone` leaves of an erase whose unlink a power cut took back.
        Ok(v) if v.is_empty() => Ok(None),
        Ok(v) => Ok(Some(v)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(StoreError::Backend(e.to_string())),
    }
}

#[cfg(target_os = "linux")]
fn delete_platform(data_dir: &Path) -> Result<(), StoreError> {
    crate::platform::durable_fs::remove_file(&data_dir.join("auth_tokens.json"))
        .map_err(|e| StoreError::Backend(e.to_string()))
}

#[cfg(test)]
#[path = "../../tests/unit/platform/secure_store.rs"]
mod tests;
