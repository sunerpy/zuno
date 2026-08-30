mod authorize;
mod discovery;
mod support;
mod token;

use reqwest::Url;
use serde::Deserialize;
use zuno_auth::ClientInfo;

pub(crate) use authorize::begin_authorization;
pub(crate) use token::{bearer_token, finish_authorization};

pub(super) const CALLBACK_PORT: u16 = 19_876;
pub(super) const CALLBACK_PATH: &str = "/mcp/oauth/callback";

#[derive(Clone)]
pub(crate) struct PendingAuthorization {
    token_endpoint: Url,
    client: ClientInfo,
    redirect_uri: String,
    resource: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct ProtectedResourceMetadata {
    pub(super) resource: String,
    #[serde(default)]
    pub(super) authorization_servers: Vec<String>,
    #[serde(default)]
    pub(super) scopes_supported: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub(super) struct AuthorizationServerMetadata {
    pub(super) authorization_endpoint: String,
    pub(super) token_endpoint: String,
    #[serde(default)]
    pub(super) registration_endpoint: Option<String>,
    #[serde(default)]
    pub(super) scopes_supported: Vec<String>,
}

#[derive(Debug)]
pub(super) struct Discovery {
    pub(super) resource: ProtectedResourceMetadata,
    pub(super) authorization: AuthorizationServerMetadata,
}
