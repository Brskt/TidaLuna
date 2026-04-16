use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MdnsDevice {
    pub addresses: Vec<String>,
    pub friendly_name: String,
    pub fullname: String,
    pub id: String,
    pub port: u16,
    #[serde(rename = "type")]
    pub device_type: DeviceType,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum DeviceType {
    TidalConnect,
}
