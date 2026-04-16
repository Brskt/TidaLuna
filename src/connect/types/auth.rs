use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ServerInfo {
    pub server_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_info: Option<AuthInfo>,
    #[serde(default)]
    pub http_header_fields: Vec<String>,
    #[serde(default)]
    pub query_parameters: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AuthInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header_auth: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth_server_info: Option<OAuthServerInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth_parameters: Option<OAuthParameters>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OAuthServerInfo {
    pub server_url: String,
    pub auth_info: OAuthAuthInfo,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub form_parameters: Option<OAuthFormParameters>,
    #[serde(default)]
    pub http_header_fields: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OAuthAuthInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header_auth: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth_parameters: Option<OAuthParameters>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OAuthParameters {
    pub access_token: String,
    pub refresh_token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) struct OAuthFormParameters {
    pub grant_type: String,
    pub scope: String,
}
