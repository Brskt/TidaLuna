use std::sync::RwLock;
use std::time::Duration;

use anyhow::{Result, anyhow};
use once_cell::sync::Lazy;
use serde::de::DeserializeOwned;

use crate::state::HTTP_CLIENT;

/// Base URL for the TIDAL desktop API (v1).
pub const API_BASE: &str = "https://desktop.tidal.com/v1";

/// Credentials received from the frontend after TIDAL auth.
#[derive(Debug, Clone, Default)]
pub struct TidalCredentials {
    pub token: String,
    pub client_id: String,
    pub country_code: String,
    pub locale: String,
}

static CREDENTIALS: Lazy<RwLock<TidalCredentials>> =
    Lazy::new(|| RwLock::new(TidalCredentials::default()));

/// Store credentials forwarded from the frontend.
///
/// Called once at startup (via IPC `tidal.set_credentials`) after the
/// frontend captures the OAuth Bearer token from TIDAL's own requests.
pub fn set_credentials(creds: TidalCredentials) {
    crate::vprintln!(
        "[API]    Credentials set — clientId: {}, country: {}",
        creds.client_id,
        creds.country_code
    );
    *CREDENTIALS.write().expect("credentials lock poisoned") = creds;
}

/// Read a snapshot of the current credentials.
pub fn credentials() -> TidalCredentials {
    CREDENTIALS
        .read()
        .expect("credentials lock poisoned")
        .clone()
}

/// Build the standard query string appended to every v1 API call.
///
/// Example: `countryCode=US&deviceType=DESKTOP&locale=en_US`
pub fn query_args() -> String {
    let creds = credentials();
    format!(
        "countryCode={}&deviceType=DESKTOP&locale={}",
        creds.country_code, creds.locale
    )
}

/// Fetch JSON from a TIDAL API URL with auth headers and retry logic.
///
/// Mirrors the TypeScript `TidalApi.fetch<T>()` behaviour:
/// - On success (2xx): deserialise and return `Ok(Some(T))`
/// - On 403 / 404: return `Ok(None)` (resource unavailable)
/// - On other errors: retry once after 1 s, then fail
pub async fn fetch_json<T: DeserializeOwned>(url: &str) -> Result<Option<T>> {
    let mut retry = true;
    loop {
        let creds = credentials();
        let resp = HTTP_CLIENT
            .get(url)
            .header("Authorization", format!("Bearer {}", creds.token))
            .header("x-tidal-token", &creds.client_id)
            .send()
            .await?;

        let status = resp.status().as_u16();

        if (200..300).contains(&status) {
            let body = resp.text().await?;
            let parsed: T = serde_json::from_str(&body)?;
            return Ok(Some(parsed));
        }

        if status == 403 || status == 404 {
            return Ok(None);
        }

        if !retry {
            return Err(anyhow!("TIDAL API {status} for {url}"));
        }

        retry = false;
        crate::vprintln!("[API]    {status} on {url} — retrying in 1s");
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}
