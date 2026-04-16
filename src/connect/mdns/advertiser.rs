use md5::{Digest, Md5};
use mdns_sd::{ServiceDaemon, ServiceInfo};
use std::sync::Arc;

use crate::connect::consts;

pub(crate) struct AdvertiseConfig {
    pub friendly_name: String,
    pub device_id: String,
    pub model_name: String,
    pub port: u16,
}

impl Default for AdvertiseConfig {
    fn default() -> Self {
        Self {
            friendly_name: format!(
                "TidaLunar: {}",
                gethostname::gethostname().to_string_lossy()
            ),
            device_id: generate_device_id(),
            model_name: "TidaLunar".to_string(),
            port: consts::WS_DEFAULT_PORT,
        }
    }
}

pub(crate) struct MdnsAdvertiser {
    daemon: Arc<ServiceDaemon>,
    registered_fullname: Option<String>,
}

impl MdnsAdvertiser {
    /// Create with an externally-owned daemon (shared with browser).
    pub fn with_daemon(daemon: Arc<ServiceDaemon>) -> Self {
        Self {
            daemon,
            registered_fullname: None,
        }
    }

    /// Create with its own daemon.
    pub fn new() -> anyhow::Result<Self> {
        let daemon = ServiceDaemon::new()?;
        Ok(Self::with_daemon(Arc::new(daemon)))
    }

    /// Start advertising. If already active, stops first.
    pub fn start(&mut self, config: &AdvertiseConfig) -> anyhow::Result<()> {
        self.stop();

        // Validate TXT entries: each key=value must be UTF-8 and <= 255 bytes
        let fn_value = truncate_txt_value(&config.friendly_name, 200);

        let properties = [
            ("fn", fn_value.as_str()),
            ("id", config.device_id.as_str()),
            ("mn", config.model_name.as_str()),
            ("ca", "0"),
            ("ve", "1"),
        ];

        let hostname = format!("{}.local.", gethostname::gethostname().to_string_lossy());
        let instance_name = format!(
            "TidalConnect-{}",
            &config.device_id[..8.min(config.device_id.len())]
        );

        let service = ServiceInfo::new(
            consts::MDNS_SERVICE_TYPE,
            &instance_name,
            &hostname,
            "", // auto-detect IP
            config.port,
            properties.as_slice(),
        )?
        .enable_addr_auto();

        let fullname = service.get_fullname().to_string();
        self.daemon.register(service)?;
        self.registered_fullname = Some(fullname.clone());

        crate::vprintln!(
            "[connect::mdns] Advertising as '{}' on port {}",
            config.friendly_name,
            config.port
        );

        Ok(())
    }

    /// Stop advertising. Sends goodbye packet (TTL=0) via unregister.
    pub fn stop(&mut self) {
        if let Some(fullname) = self.registered_fullname.take() {
            // unregister() sends goodbye synchronously before returning
            let _ = self.daemon.unregister(&fullname);
            crate::vprintln!("[connect::mdns] Stopped advertising");
        }
    }

    /// Borrow the shared daemon so callers can drive a bounded shutdown
    /// through `MdnsBackend`. The daemon thread is not owned exclusively
    /// by the advertiser, so shutdown is the caller's responsibility.
    pub(crate) fn daemon(&self) -> Arc<ServiceDaemon> {
        self.daemon.clone()
    }
}

/// Generate a device ID from hostname using MD5 (matching the original binary behavior).
fn generate_device_id() -> String {
    let hostname = gethostname::gethostname().to_string_lossy().to_string();
    let mut hasher = Md5::new();
    hasher.update(hostname.as_bytes());
    let result = hasher.finalize();
    format!("{:032x}", u128::from_be_bytes(result.into()))
}

/// Truncate a string to fit within the DNS-SD TXT record limit (key=value <= 255 bytes).
fn truncate_txt_value(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    // Truncate at a char boundary
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_string()
}
