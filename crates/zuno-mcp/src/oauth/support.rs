use std::time::{SystemTime, UNIX_EPOCH};

use reqwest::Url;
use zuno_auth::{ClientInfo, Secret};
use zuno_config::schema::mcp::{McpOauth, McpOauthConfig, McpRemote};

use crate::remote::RemoteError;

use super::{CALLBACK_PATH, CALLBACK_PORT};

pub(super) fn oauth_client(
    server: &str,
    config: &McpRemote,
) -> Result<reqwest::Client, RemoteError> {
    let timeout = config
        .timeout
        .map_or(crate::stdio::DEFAULT_REQUEST_TIMEOUT, |value| {
            std::time::Duration::from_millis(u64::from(value.get()))
        });
    reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|error| oauth_error(server, error))
}

pub(super) fn oauth_config(config: &McpRemote) -> Option<&McpOauthConfig> {
    match &config.oauth {
        Some(McpOauth::Config(settings)) => Some(settings),
        Some(McpOauth::Disabled(_)) | None => None,
    }
}

pub(super) fn configured_client(config: Option<&McpOauthConfig>) -> Option<ClientInfo> {
    let config = config?;
    Some(ClientInfo {
        client_id: config.client_id.clone()?,
        client_secret: config.client_secret.clone().map(Secret::new),
        client_id_issued_at: None,
        client_secret_expires_at: None,
    })
}

pub(super) fn client_is_valid(client: &ClientInfo) -> bool {
    client
        .client_secret_expires_at
        .is_none_or(|expiry| expiry == 0 || expiry > unix_seconds())
}

pub(super) fn redirect_uri(config: Option<&McpOauthConfig>) -> String {
    if let Some(uri) = config.and_then(|settings| settings.redirect_uri.clone()) {
        return uri;
    }
    let port = config
        .and_then(|settings| settings.callback_port)
        .map_or(CALLBACK_PORT, std::num::NonZeroU16::get);
    format!("http://127.0.0.1:{port}{CALLBACK_PATH}")
}

pub(super) fn challenge_parameter(challenge: &str, name: &str) -> Option<String> {
    let marker = format!("{name}=");
    let start = challenge.find(&marker)? + marker.len();
    let value = challenge[start..].trim_start();
    if let Some(value) = value.strip_prefix('"') {
        return value.find('"').map(|end| value[..end].to_owned());
    }
    Some(
        value
            .split([',', ' '])
            .next()
            .unwrap_or_default()
            .to_owned(),
    )
}

pub(super) fn random_hex(bytes: usize) -> String {
    let mut output = String::with_capacity(bytes * 2);
    while output.len() < bytes * 2 {
        output.push_str(&uuid::Uuid::new_v4().simple().to_string());
    }
    output.truncate(bytes * 2);
    output
}

pub(super) fn server_origin(url: &Url) -> String {
    let host = url.host_str().unwrap_or_default();
    match url.port() {
        Some(port) => format!("{}://{host}:{port}", url.scheme()),
        None => format!("{}://{host}", url.scheme()),
    }
}

pub(super) fn parse_url(server: &str, value: &str, label: &str) -> Result<Url, RemoteError> {
    Url::parse(value).map_err(|error| RemoteError::OAuth {
        server: server.to_owned(),
        message: format!("invalid {label}: {error}"),
    })
}

pub(super) fn unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
        })
}

pub(super) fn oauth_error(server: &str, error: impl std::fmt::Display) -> RemoteError {
    RemoteError::OAuth {
        server: server.to_owned(),
        message: error.to_string(),
    }
}
