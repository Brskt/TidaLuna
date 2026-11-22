use crate::app_state::{AppState, with_state};

/// Real OAuth tokens in play paired with the opaque they map to. Single source for
/// `scrub_real_tokens` (uses both halves) and `leaks_real_token` (uses the real).
fn real_token_pairs(state: &AppState) -> Vec<(String, String)> {
    let mut pairs: Vec<(String, String)> = Vec::new();
    if let Some(ref ts) = state.token_state {
        pairs.push((
            ts.current.access_token.clone(),
            ts.current.opaque_at.clone(),
        ));
        pairs.push((
            ts.current.refresh_token.clone(),
            ts.current.opaque_rt.clone(),
        ));
        if let Some(ref prev) = ts.previous {
            pairs.push((prev.access_token.clone(), prev.opaque_at.clone()));
            pairs.push((prev.refresh_token.clone(), prev.opaque_rt.clone()));
        }
    }
    pairs
}

/// True if `url`/`payload` contains any real OAuth token, however the plugin got it.
pub(super) fn leaks_real_token(url: &str, payload: Option<&str>) -> bool {
    with_state(|state| {
        real_token_pairs(state).iter().any(|(real, _)| {
            !real.is_empty()
                && (url.contains(real.as_str())
                    || payload.is_some_and(|p| p.contains(real.as_str())))
        })
    })
    .unwrap_or(false)
}

/// Replace any real OAuth token present in `text` with its opaque counterpart.
/// `pairs` is (real_token, replacement), gathered from the token state.
fn scrub_real_tokens_with(text: String, pairs: &[(String, String)]) -> String {
    let mut out = text;
    for (real, replacement) in pairs {
        // Skip values under 8 bytes: a substring match would corrupt a body that merely
        // contains a few common characters.
        if real.len() < 8 {
            continue;
        }
        if out.contains(real.as_str()) {
            crate::vprintln!("[PLUGIN:FETCH] scrubbed a real token from a plugin-facing response");
            out = out.replace(real.as_str(), replacement);
        }
    }
    out
}

/// Replace any real OAuth token in a plugin-facing response with its opaque nonce. TIDAL APIs
/// don't echo the bearer: this is normally a no-op.
pub(crate) fn scrub_real_tokens(text: String) -> String {
    let pairs = with_state(|state| real_token_pairs(state)).unwrap_or_default();
    scrub_real_tokens_with(text, &pairs)
}

#[cfg(test)]
#[path = "../../../tests/unit/ipc/plugin/scrub.rs"]
mod tests;
