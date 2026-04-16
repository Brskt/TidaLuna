//! Module-wide constants for TIDAL Connect.

// mDNS
pub const MDNS_SERVICE_TYPE: &str = "_tidalconnect._tcp.local.";

// WebSocket
pub const WS_DEFAULT_PORT: u16 = 9000;
pub const PING_INTERVAL_MS: u64 = 15_000;
pub const PING_TIMEOUT_MS: u64 = 31_000;

// Session
pub const SESSION_APP_ID: &str = "tidal";
pub const SESSION_APP_NAME: &str = "tidal";

// HTTP
pub const HTTP_TIMEOUT_SECS: u64 = 120;
