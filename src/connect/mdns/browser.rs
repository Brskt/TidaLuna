use mdns_sd::{ServiceDaemon, ServiceEvent};
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::connect::consts;
use crate::connect::types::{DeviceType, MdnsDevice};

pub(crate) enum BrowserEvent {
    DeviceFound(MdnsDevice),
    DeviceRemoved { fullname: String },
}

pub(crate) struct MdnsBrowser {
    daemon: Arc<ServiceDaemon>,
    event_tx: mpsc::Sender<BrowserEvent>,
    browse_task: Option<tokio::task::JoinHandle<()>>,
    filter_local: bool,
}

impl MdnsBrowser {
    /// Create with an externally-owned daemon (shared with advertiser).
    pub fn with_daemon(
        daemon: Arc<ServiceDaemon>,
        event_tx: mpsc::Sender<BrowserEvent>,
        filter_local: bool,
    ) -> Self {
        Self {
            daemon,
            event_tx,
            browse_task: None,
            filter_local,
        }
    }

    /// Create with its own daemon.
    pub fn new(event_tx: mpsc::Sender<BrowserEvent>, filter_local: bool) -> anyhow::Result<Self> {
        let daemon = ServiceDaemon::new()?;
        Ok(Self::with_daemon(Arc::new(daemon), event_tx, filter_local))
    }

    /// Start discovery. If already running, stops first then restarts.
    pub fn start_discovery(&mut self) -> anyhow::Result<()> {
        self.stop_discovery();

        let receiver = self.daemon.browse(consts::MDNS_SERVICE_TYPE)?;
        let tx = self.event_tx.clone();
        let filter_local = self.filter_local;

        let rt = crate::state::RT_HANDLE
            .get()
            .ok_or_else(|| anyhow::anyhow!("Tokio runtime not available"))?;
        let handle = rt.spawn(async move {
            let local_ips = if filter_local {
                get_local_ipv4s()
            } else {
                Vec::new()
            };

            while let Ok(event) = receiver.recv_async().await {
                match event {
                    ServiceEvent::ServiceResolved(info) => {
                        if let Some(device) = service_info_to_device(&info) {
                            if filter_local && is_local_device(&device, &local_ips) {
                                continue;
                            }
                            let _ = tx.send(BrowserEvent::DeviceFound(device)).await;
                        }
                    }
                    ServiceEvent::ServiceRemoved(_service_type, fullname) => {
                        // Use fullname as key (TXT id != instance name)
                        let _ = tx.send(BrowserEvent::DeviceRemoved { fullname }).await;
                    }
                    ServiceEvent::SearchStarted(_) => {
                        crate::vprintln!("[connect::mdns] Discovery started");
                    }
                    ServiceEvent::SearchStopped(_) => {
                        crate::vprintln!("[connect::mdns] Discovery stopped");
                        break;
                    }
                    ServiceEvent::ServiceFound(_, _) => {
                        // Wait for ServiceResolved for full details
                    }
                    _ => {}
                }
            }
        });

        self.browse_task = Some(handle);
        Ok(())
    }

    /// Stop discovery. Only stops browse, does not shut down the daemon.
    pub fn stop_discovery(&mut self) {
        if let Some(handle) = self.browse_task.take() {
            let _ = self.daemon.stop_browse(consts::MDNS_SERVICE_TYPE);
            handle.abort();
        }
    }

    /// Refresh: stop then restart.
    pub fn refresh(&mut self) -> anyhow::Result<()> {
        self.stop_discovery();
        self.start_discovery()
    }
}

fn service_info_to_device(info: &mdns_sd::ResolvedService) -> Option<MdnsDevice> {
    let addresses: Vec<String> = info
        .get_addresses_v4()
        .iter()
        .map(|ip| ip.to_string())
        .collect();

    if addresses.is_empty() {
        return None;
    }

    let friendly_name = info
        .get_property_val_str("fn")
        .unwrap_or("Unknown")
        .to_string();
    let id = info
        .get_property_val_str("id")
        .unwrap_or_default()
        .to_string();

    Some(MdnsDevice {
        addresses,
        friendly_name,
        fullname: info.get_fullname().to_string(),
        id,
        port: info.get_port(),
        device_type: DeviceType::TidalConnect,
    })
}

fn get_local_ipv4s() -> Vec<String> {
    // Use gethostname + DNS resolution as a simple cross-platform approach.
    // On most systems, the hostname resolves to the local IPs.
    let mut ips = Vec::new();

    // Fallback: common local addresses
    if let Ok(hostname) = std::net::ToSocketAddrs::to_socket_addrs(&(
        gethostname::gethostname().to_string_lossy().to_string(),
        0u16,
    )) {
        for addr in hostname {
            if let std::net::SocketAddr::V4(v4) = addr {
                ips.push(v4.ip().to_string());
            }
        }
    }

    // Always include loopback
    ips.push("127.0.0.1".to_string());
    ips
}

fn is_local_device(device: &MdnsDevice, local_ips: &[String]) -> bool {
    device.addresses.iter().any(|addr| local_ips.contains(addr))
}
