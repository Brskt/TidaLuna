use oauth2::{PkceCodeChallenge, PkceCodeVerifier};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PkceCredentials {
    pub credentials_storage_key: String,
    pub code_challenge: String,
    pub redirect_uri: String,
    pub code_verifier: String,
}

fn pkce_credentials_path(data_dir: &Path) -> PathBuf {
    data_dir.join("pkce_credentials.json")
}

fn generate_pkce_credentials() -> PkceCredentials {
    let (code_challenge, code_verifier) = PkceCodeChallenge::new_random_sha256();

    PkceCredentials {
        credentials_storage_key: "tidal".to_string(),
        code_challenge: code_challenge.as_str().to_string(),
        redirect_uri: "tidal://login/auth".to_string(),
        code_verifier: code_verifier.secret().to_string(),
    }
}

fn is_valid_pkce_verifier(verifier: &str) -> bool {
    let len = verifier.len();
    (43..=128).contains(&len)
        && verifier
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~'))
}

fn is_valid_pkce_credentials(c: &PkceCredentials) -> bool {
    if c.credentials_storage_key.trim().is_empty()
        || c.code_challenge.trim().is_empty()
        || c.redirect_uri.trim().is_empty()
        || c.code_verifier.trim().is_empty()
        || !is_valid_pkce_verifier(&c.code_verifier)
    {
        return false;
    }

    let verifier = PkceCodeVerifier::new(c.code_verifier.clone());
    let expected_challenge = PkceCodeChallenge::from_code_verifier_sha256(&verifier);
    c.code_challenge == expected_challenge.as_str()
}

#[allow(dead_code)]
pub(crate) fn clear_persisted_credentials(data_dir: &Path) {
    let path = pkce_credentials_path(data_dir);
    match std::fs::remove_file(&path) {
        Ok(()) => crate::vprintln!("[PKCE]   Deleted persisted credentials"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            crate::vprintln!("[PKCE]   No persisted credentials to delete");
        }
        Err(e) => crate::vprintln!("[PKCE]   Failed to delete credentials: {e}"),
    }
}

pub(crate) fn load_or_create_pkce_credentials(data_dir: &Path) -> PkceCredentials {
    let path = pkce_credentials_path(data_dir);

    if let Ok(bytes) = std::fs::read(&path)
        && let Ok(mut loaded) = serde_json::from_slice::<PkceCredentials>(&bytes)
    {
        if loaded.credentials_storage_key.trim().is_empty() {
            loaded.credentials_storage_key = "tidal".to_string();
        }
        // Always enforce the correct redirect_uri (migration from tidal://auth/)
        loaded.redirect_uri = "tidal://login/auth".to_string();
        if is_valid_pkce_credentials(&loaded) {
            crate::vprintln!("[PKCE]   Loaded persisted credentials");
            return loaded;
        }
        crate::vprintln!("[PKCE]   Invalid persisted credentials, regenerating");
    }

    let generated = generate_pkce_credentials();
    if let Some(parent) = path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        eprintln!(
            "[PKCE]   Failed to create directory {}: {e}",
            parent.display()
        );
        return generated;
    }
    match serde_json::to_vec_pretty(&generated) {
        Ok(json) => {
            if let Err(e) = std::fs::write(&path, json) {
                eprintln!("[PKCE]   Failed to persist credentials: {e}");
            } else {
                crate::vprintln!("[PKCE]   Persisted credentials");
            }
        }
        Err(e) => eprintln!("[PKCE]   Failed to serialize credentials: {e}"),
    }
    generated
}
