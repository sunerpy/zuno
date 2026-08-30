use std::collections::BTreeMap;
use std::num::NonZeroU16;

use serde_json::json;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};
use zuno_auth::McpAuthStore;
use zuno_config::schema::mcp::{McpOauth, McpOauthConfig};
use zuno_mcp::{RemoteClient, RemoteConnect};

use crate::remote_support::{self, initialize_result, remote_config};

#[tokio::test]
async fn remote_configured_oauth_exchanges_code_with_secret_and_explicit_redirect() {
    let server = MockServer::start().await;
    let resource_url = format!("{}/oauth-resource", server.uri());
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .respond_with(ResponseTemplate::new(401).insert_header(
            "www-authenticate",
            format!("Bearer resource_metadata=\"{resource_url}\""),
        ))
        .with_priority(10)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .and(header("authorization", "Bearer configured-access"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_json(json!({
                    "jsonrpc": "2.0", "id": 1,
                    "result": initialize_result("2025-03-26", "configured-oauth"),
                })),
        )
        .with_priority(1)
        .mount(&server)
        .await;
    remote_support::mount_oauth_discovery(&server).await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "configured-access", "refresh_token": "configured-refresh",
            "expires_in": 3600, "scope": "configured.scope",
        })))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().expect("auth tempdir");
    let store = McpAuthStore::new(dir.path().join("mcp-auth.json"));
    let redirect = "http://127.0.0.1:43123/custom/callback";
    let mut config = remote_config(format!("{}/mcp", server.uri()));
    config.oauth = Some(McpOauth::Config(McpOauthConfig {
        client_id: Some("configured-client".to_owned()),
        client_secret: Some("configured-secret".to_owned()),
        scope: Some("configured.scope".to_owned()),
        callback_port: NonZeroU16::new(31_987),
        redirect_uri: Some(redirect.to_owned()),
    }));
    let outcome = RemoteClient::connect_with_store("configured", &config, store.clone())
        .await
        .expect("configured OAuth begins");
    let RemoteConnect::AuthorizationRequired(request) = outcome else {
        panic!("authorization is required")
    };
    let authorization = url::Url::parse(request.authorization_url()).expect("authorization URL");
    let query: BTreeMap<_, _> = authorization.query_pairs().into_owned().collect();
    assert_eq!(
        query.get("client_id").map(String::as_str),
        Some("configured-client")
    );
    assert_eq!(
        query.get("redirect_uri").map(String::as_str),
        Some(redirect)
    );
    assert_eq!(
        query.get("scope").map(String::as_str),
        Some("configured.scope")
    );
    let state = query.get("state").expect("state").clone();
    assert!(!format!("{request:?}").contains("configured-secret"));

    let completed = request
        .finish("authorization-code", &state)
        .await
        .expect("code exchange and authenticated reconnect succeed");
    let RemoteConnect::Connected(client) = completed else {
        panic!("code exchange must reconnect")
    };
    assert_eq!(client.initialization().server_info.name, "configured-oauth");
    let stored = store
        .get("configured")
        .expect("read store")
        .expect("stored tokens");
    assert_eq!(
        stored
            .tokens
            .as_ref()
            .map(|tokens| tokens.access_token.expose()),
        Some("configured-access")
    );
    assert!(stored.code_verifier.is_none());
    assert!(stored.oauth_state.is_none());
    let requests = server.received_requests().await.expect("request journal");
    assert!(
        !requests
            .iter()
            .any(|request| request.url.path() == "/register")
    );
    let token_request = requests
        .iter()
        .find(|request| request.url.path() == "/token")
        .expect("token request");
    let form = form_body(&token_request.body);
    assert_eq!(
        form.get("grant_type").map(String::as_str),
        Some("authorization_code")
    );
    assert_eq!(
        form.get("code").map(String::as_str),
        Some("authorization-code")
    );
    assert_eq!(
        form.get("client_id").map(String::as_str),
        Some("configured-client")
    );
    assert_eq!(
        form.get("client_secret").map(String::as_str),
        Some("configured-secret")
    );
    assert_eq!(form.get("redirect_uri").map(String::as_str), Some(redirect));
    assert!(
        form.get("code_verifier")
            .is_some_and(|value| !value.is_empty())
    );
}

fn form_body(body: &[u8]) -> BTreeMap<String, String> {
    url::form_urlencoded::parse(body).into_owned().collect()
}
