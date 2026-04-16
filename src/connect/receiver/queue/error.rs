//! Errors raised by the queue subsystem.

/// Unified error type for the queue subsystem. Each variant distinguishes a
/// failure mode so the caller can react appropriately (e.g. refresh the
/// token on `TokenExpired`, relogin on `AuthTerminated`, retry on
/// `HttpStatus` with backoff).
#[derive(Debug, Clone)]
pub(crate) enum QueueError {
    /// No server info configured (content_server/queue_server is None).
    NoServer,
    /// OAuth server info missing, cannot refresh token.
    NoOAuthServer,
    /// HTTP network error (transport failure, timeout, DNS, etc.).
    Network(String),
    /// HTTP responded with a non-success status. The 401 case is distinct
    /// because it triggers the token-refresh flow.
    HttpStatus(u16),
    /// 401 on a regular request; caller should refresh the token.
    TokenExpired,
    /// Failed to parse JSON response.
    InvalidResponse(String),
    /// Required field missing in response body.
    MissingField(&'static str),
    /// Refresh was rejected by the authorization server with `invalid_grant`
    /// (RFC 6749 §5.2). The current generation has been transitioned to
    /// `Terminated(InvalidGrant)` in `AuthStore`. The caller must NOT retry
    /// the refresh and should surface a relogin prompt exactly once per
    /// generation.
    AuthTerminated { provider_error: String },
}

impl std::fmt::Display for QueueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            QueueError::NoServer => f.write_str("no server configured"),
            QueueError::NoOAuthServer => f.write_str("no OAuth server info"),
            QueueError::Network(e) => write!(f, "network: {e}"),
            QueueError::HttpStatus(s) => write!(f, "HTTP {s}"),
            QueueError::TokenExpired => f.write_str("token expired (401)"),
            QueueError::InvalidResponse(e) => write!(f, "invalid response: {e}"),
            QueueError::MissingField(k) => write!(f, "missing field: {k}"),
            QueueError::AuthTerminated { provider_error } => {
                write!(f, "auth terminated: {provider_error}")
            }
        }
    }
}

impl std::error::Error for QueueError {}
