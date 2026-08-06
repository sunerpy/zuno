use oc_config::schema::mcp::McpRemote;
use reqwest::Url;

use crate::remote::RemoteError;

use super::support::{challenge_parameter, parse_url, server_origin};
use super::{AuthorizationServerMetadata, Discovery, ProtectedResourceMetadata};

pub(super) async fn discover(
    server: &str,
    config: &McpRemote,
    http: &reqwest::Client,
    challenge: Option<&str>,
) -> Result<Discovery, RemoteError> {
    let server_url = parse_url(server, &config.url, "server URL")?;
    let challenge_metadata =
        challenge.and_then(|value| challenge_parameter(value, "resource_metadata"));
    let resource_candidates = if let Some(metadata) = challenge_metadata {
        vec![parse_url(server, &metadata, "resource metadata URL")?]
    } else {
        protected_resource_candidates(&server_url)
    };
    let mut resource = None;
    for candidate in resource_candidates {
        let response = match http.get(candidate).send().await {
            Ok(response) if response.status().is_success() => response,
            Ok(_) | Err(_) => continue,
        };
        if let Ok(metadata) = response.json::<ProtectedResourceMetadata>().await {
            resource = Some(metadata);
            break;
        }
    }
    let resource = resource.unwrap_or_else(|| ProtectedResourceMetadata {
        resource: config.url.clone(),
        authorization_servers: vec![server_origin(&server_url)],
        scopes_supported: Vec::new(),
    });
    let authorization_server = resource
        .authorization_servers
        .first()
        .cloned()
        .unwrap_or_else(|| server_origin(&server_url));
    let authorization_server = parse_url(server, &authorization_server, "authorization server")?;
    let mut authorization = None;
    for candidate in authorization_metadata_candidates(&authorization_server) {
        let response = match http.get(candidate).send().await {
            Ok(response) if response.status().is_success() => response,
            Ok(_) | Err(_) => continue,
        };
        if let Ok(metadata) = response.json::<AuthorizationServerMetadata>().await {
            authorization = Some(metadata);
            break;
        }
    }
    let authorization = authorization.ok_or_else(|| RemoteError::OAuth {
        server: server.to_owned(),
        message: "authorization server metadata discovery failed".to_owned(),
    })?;
    Ok(Discovery {
        resource,
        authorization,
    })
}

fn protected_resource_candidates(server: &Url) -> Vec<Url> {
    let origin = server_origin(server);
    let path = server.path().trim_start_matches('/');
    let mut candidates = Vec::new();
    if !path.is_empty()
        && let Ok(url) = Url::parse(&format!(
            "{origin}/.well-known/oauth-protected-resource/{path}"
        ))
    {
        candidates.push(url);
    }
    if let Ok(url) = Url::parse(&format!("{origin}/.well-known/oauth-protected-resource")) {
        candidates.push(url);
    }
    candidates
}

fn authorization_metadata_candidates(server: &Url) -> Vec<Url> {
    let origin = server_origin(server);
    let path = server.path().trim_matches('/');
    let suffix = if path.is_empty() {
        String::new()
    } else {
        format!("/{path}")
    };
    [
        format!("{origin}/.well-known/oauth-authorization-server{suffix}"),
        format!("{origin}/.well-known/openid-configuration{suffix}"),
    ]
    .into_iter()
    .filter_map(|value| Url::parse(&value).ok())
    .collect()
}
