use std::collections::BTreeMap;

use oc_auth::{McpAuthStore, Secret, Tokens};
use oc_config::schema::mcp::McpRemote;
use serde::Deserialize;

use crate::remote::{RemoteConnect, RemoteError};

use super::PendingAuthorization;
use super::discovery::discover;
use super::support::{
    configured_client, oauth_client, oauth_config, oauth_error, parse_url, unix_seconds,
};

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
    #[serde(default)]
    scope: Option<String>,
}

pub(crate) async fn finish_authorization(
    server: String,
    config: McpRemote,
    store: McpAuthStore,
    pending: PendingAuthorization,
    authorization_code: &str,
    returned_state: &str,
) -> Result<RemoteConnect, RemoteError> {
    let entry = store
        .get(&server)
        .map_err(|error| oauth_error(&server, error))?
        .ok_or_else(|| pending_error(&server, "no pending OAuth state"))?;
    let stored_state = entry
        .oauth_state
        .ok_or_else(|| pending_error(&server, "no pending OAuth state"))?;
    if stored_state.expose() != returned_state {
        store
            .clear_oauth_state(&server)
            .map_err(|error| oauth_error(&server, error))?;
        return Err(pending_error(&server, "OAuth state mismatch"));
    }
    let verifier = entry
        .code_verifier
        .ok_or_else(|| pending_error(&server, "no PKCE verifier was saved"))?;
    let mut form = BTreeMap::from([
        ("grant_type", "authorization_code".to_owned()),
        ("code", authorization_code.to_owned()),
        ("redirect_uri", pending.redirect_uri),
        ("client_id", pending.client.client_id.clone()),
        ("code_verifier", verifier.expose().to_owned()),
        ("resource", pending.resource),
    ]);
    if let Some(secret) = &pending.client.client_secret {
        form.insert("client_secret", secret.expose().to_owned());
    }
    let response = oauth_client(&server, &config)?
        .post(pending.token_endpoint)
        .form(&form)
        .send()
        .await
        .map_err(|error| oauth_error(&server, error))?;
    let token = decode_token_response(&server, response).await?;
    store_tokens(&server, &config.url, &store, token, None)?;
    store
        .clear_code_verifier(&server)
        .map_err(|error| oauth_error(&server, error))?;
    store
        .clear_oauth_state(&server)
        .map_err(|error| oauth_error(&server, error))?;
    crate::remote::RemoteClient::connect_with_store(server, &config, store).await
}

pub(crate) async fn bearer_token(
    server: &str,
    config: &McpRemote,
    store: &McpAuthStore,
) -> Result<Option<Secret>, RemoteError> {
    let Some(entry) = store
        .get_for_url(server, &config.url)
        .map_err(|error| oauth_error(server, error))?
    else {
        return Ok(None);
    };
    let Some(tokens) = entry.tokens else {
        return Ok(None);
    };
    if tokens
        .expires_at
        .is_none_or(|expiry| expiry > unix_seconds() + 30)
    {
        return Ok(Some(tokens.access_token));
    }
    let Some(refresh) = tokens.refresh_token.as_ref() else {
        return Ok(Some(tokens.access_token));
    };
    let http = oauth_client(server, config)?;
    let discovery = discover(server, config, &http, None).await?;
    let oauth = oauth_config(config);
    let client = configured_client(oauth)
        .or(entry.client_info)
        .ok_or_else(|| {
            pending_error(
                server,
                "refresh token exists without OAuth client information",
            )
        })?;
    let mut form = BTreeMap::from([
        ("grant_type", "refresh_token".to_owned()),
        ("refresh_token", refresh.expose().to_owned()),
        ("client_id", client.client_id.clone()),
        ("resource", discovery.resource.resource),
    ]);
    if let Some(secret) = &client.client_secret {
        form.insert("client_secret", secret.expose().to_owned());
    }
    if let Some(scope) = oauth.and_then(|settings| settings.scope.clone()) {
        form.insert("scope", scope);
    }
    let token_endpoint = parse_url(
        server,
        &discovery.authorization.token_endpoint,
        "token endpoint",
    )?;
    let response = http
        .post(token_endpoint)
        .form(&form)
        .send()
        .await
        .map_err(|error| oauth_error(server, error))?;
    let token = decode_token_response(server, response).await?;
    let access = Secret::new(token.access_token.clone());
    store_tokens(server, &config.url, store, token, Some(refresh.clone()))?;
    Ok(Some(access))
}

async fn decode_token_response(
    server: &str,
    response: reqwest::Response,
) -> Result<TokenResponse, RemoteError> {
    if !response.status().is_success() {
        return Err(pending_error(
            server,
            &format!("token endpoint returned HTTP {}", response.status()),
        ));
    }
    response
        .json()
        .await
        .map_err(|error| oauth_error(server, error))
}

fn store_tokens(
    server: &str,
    server_url: &str,
    store: &McpAuthStore,
    response: TokenResponse,
    fallback_refresh: Option<Secret>,
) -> Result<(), RemoteError> {
    let tokens = Tokens {
        access_token: Secret::new(response.access_token),
        refresh_token: response.refresh_token.map(Secret::new).or(fallback_refresh),
        expires_at: response.expires_in.map(|seconds| unix_seconds() + seconds),
        scope: response.scope,
    };
    store
        .update_tokens(server, tokens, Some(server_url))
        .map_err(|error| oauth_error(server, error))
}

fn pending_error(server: &str, message: &str) -> RemoteError {
    RemoteError::OAuth {
        server: server.to_owned(),
        message: message.to_owned(),
    }
}
