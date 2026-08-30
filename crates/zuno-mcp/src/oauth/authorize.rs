use base64::Engine as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zuno_auth::{ClientInfo, McpAuthStore, Secret};
use zuno_config::schema::mcp::McpRemote;

use crate::remote::{AuthorizationRequest, RemoteConnect, RemoteError};

use super::discovery::discover;
use super::support::{
    challenge_parameter, client_is_valid, configured_client, oauth_client, oauth_config,
    oauth_error, parse_url, random_hex, redirect_uri,
};
use super::{Discovery, PendingAuthorization};

#[derive(Serialize)]
struct ClientMetadata {
    redirect_uris: Vec<String>,
    client_name: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    client_uri: Option<&'static str>,
    grant_types: [&'static str; 2],
    response_types: [&'static str; 1],
    token_endpoint_auth_method: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    scope: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RegistrationResponse {
    client_id: String,
    #[serde(default)]
    client_secret: Option<String>,
    #[serde(default)]
    client_id_issued_at: Option<i64>,
    #[serde(default)]
    client_secret_expires_at: Option<i64>,
}

pub(crate) async fn begin_authorization(
    server: &str,
    config: &McpRemote,
    store: McpAuthStore,
    challenge: Option<&str>,
) -> Result<RemoteConnect, RemoteError> {
    let oauth = oauth_config(config);
    let redirect_uri = redirect_uri(oauth);
    let http = oauth_client(server, config)?;
    let discovery = discover(server, config, &http, challenge).await?;
    let challenged_scope = challenge.and_then(|value| challenge_parameter(value, "scope"));
    let client =
        client_information(server, config, &store, &http, &discovery, &redirect_uri).await?;
    let verifier = Secret::new(random_hex(64));
    let state = Secret::new(random_hex(32));
    store
        .update_code_verifier(server, verifier.clone())
        .map_err(|error| oauth_error(server, error))?;
    store
        .update_oauth_state(server, state.clone())
        .map_err(|error| oauth_error(server, error))?;

    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(Sha256::digest(verifier.expose().as_bytes()));
    let scope = oauth
        .and_then(|settings| settings.scope.clone())
        .or(challenged_scope)
        .or_else(|| {
            (!discovery.resource.scopes_supported.is_empty())
                .then(|| discovery.resource.scopes_supported.join(" "))
        })
        .or_else(|| {
            (!discovery.authorization.scopes_supported.is_empty())
                .then(|| discovery.authorization.scopes_supported.join(" "))
        });
    let mut authorization_url = parse_url(
        server,
        &discovery.authorization.authorization_endpoint,
        "authorization endpoint",
    )?;
    {
        let mut query = authorization_url.query_pairs_mut();
        query
            .append_pair("response_type", "code")
            .append_pair("client_id", &client.client_id)
            .append_pair("redirect_uri", &redirect_uri)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", state.expose())
            .append_pair("resource", &discovery.resource.resource);
        if let Some(scope) = &scope {
            query.append_pair("scope", scope);
        }
    }
    let pending = PendingAuthorization {
        token_endpoint: parse_url(
            server,
            &discovery.authorization.token_endpoint,
            "token endpoint",
        )?,
        client,
        redirect_uri,
        resource: discovery.resource.resource,
    };
    Ok(RemoteConnect::AuthorizationRequired(Box::new(
        AuthorizationRequest::new(
            authorization_url.into(),
            server.to_owned(),
            config.clone(),
            store,
            pending,
        ),
    )))
}

async fn client_information(
    server: &str,
    config: &McpRemote,
    store: &McpAuthStore,
    http: &reqwest::Client,
    discovery: &Discovery,
    redirect_uri: &str,
) -> Result<ClientInfo, RemoteError> {
    let oauth = oauth_config(config);
    if let Some(client) = configured_client(oauth) {
        return Ok(client);
    }
    if let Some(client) = store
        .get_for_url(server, &config.url)
        .map_err(|error| oauth_error(server, error))?
        .and_then(|entry| entry.client_info)
        .filter(client_is_valid)
    {
        return Ok(client);
    }
    let endpoint = discovery
        .authorization
        .registration_endpoint
        .as_ref()
        .ok_or_else(|| RemoteError::OAuth {
            server: server.to_owned(),
            message: "authorization server does not support dynamic client registration; configure clientId"
                .to_owned(),
        })?;
    let metadata = ClientMetadata {
        redirect_uris: vec![redirect_uri.to_owned()],
        client_name: "Zuno",
        client_uri: None,
        grant_types: ["authorization_code", "refresh_token"],
        response_types: ["code"],
        token_endpoint_auth_method: "none",
        scope: oauth.and_then(|settings| settings.scope.clone()),
    };
    let response = http
        .post(parse_url(server, endpoint, "registration endpoint")?)
        .json(&metadata)
        .send()
        .await
        .map_err(|error| oauth_error(server, error))?;
    if !response.status().is_success() {
        return Err(RemoteError::OAuth {
            server: server.to_owned(),
            message: format!(
                "dynamic client registration returned HTTP {}",
                response.status()
            ),
        });
    }
    let registered = response
        .json::<RegistrationResponse>()
        .await
        .map_err(|error| oauth_error(server, error))?;
    let client = ClientInfo {
        client_id: registered.client_id,
        client_secret: registered.client_secret.map(Secret::new),
        client_id_issued_at: registered.client_id_issued_at,
        client_secret_expires_at: registered.client_secret_expires_at,
    };
    store
        .update_client_info(server, client.clone(), Some(&config.url))
        .map_err(|error| oauth_error(server, error))?;
    Ok(client)
}
