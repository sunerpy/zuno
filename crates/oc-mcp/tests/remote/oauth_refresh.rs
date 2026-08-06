use std::collections::BTreeMap;

use oc_auth::{ClientInfo, Entry, McpAuthStore, Secret, Tokens};
use oc_mcp::{RemoteClient, RemoteConnect};
use serde_json::json;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::remote_support::{self, initialize_result, remote_config};

#[tokio::test]
async fn remote_expired_token_refreshes_before_the_mcp_handshake() {
    let server = MockServer::start().await;
    let url = format!("{}/mcp", server.uri());
    remote_support::mount_default_oauth_discovery(&server).await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "fresh-access", "expires_in": 3600, "scope": "tools.read",
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .and(header("authorization", "Bearer fresh-access"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_json(json!({
                    "jsonrpc": "2.0", "id": 1,
                    "result": initialize_result("2025-03-26", "refreshed-server"),
                })),
        )
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().expect("auth tempdir");
    let store = McpAuthStore::new(dir.path().join("mcp-auth.json"));
    store
        .set(
            "refresh",
            Entry {
                tokens: Some(Tokens {
                    access_token: Secret::new("expired-access"),
                    refresh_token: Some(Secret::new("persisted-refresh")),
                    expires_at: Some(1),
                    scope: Some("tools.read".to_owned()),
                }),
                client_info: Some(ClientInfo {
                    client_id: "refresh-client".to_owned(),
                    client_secret: Some(Secret::new("refresh-secret")),
                    client_id_issued_at: None,
                    client_secret_expires_at: Some(0),
                }),
                ..Entry::default()
            },
            Some(&url),
        )
        .expect("seed expired credentials");
    let outcome = RemoteClient::connect_with_store("refresh", &remote_config(url), store.clone())
        .await
        .expect("refresh and connect succeed");
    let RemoteConnect::Connected(client) = outcome else {
        panic!("refresh must avoid browser authorization")
    };
    assert_eq!(client.initialization().server_info.name, "refreshed-server");
    let stored = store.get("refresh").expect("read store").expect("entry");
    let tokens = stored.tokens.expect("tokens");
    assert_eq!(tokens.access_token.expose(), "fresh-access");
    assert_eq!(
        tokens.refresh_token.as_ref().map(Secret::expose),
        Some("persisted-refresh")
    );
    let requests = server.received_requests().await.expect("request journal");
    let token_request = requests
        .iter()
        .find(|request| request.url.path() == "/token")
        .expect("refresh request");
    let form = form_body(&token_request.body);
    assert_eq!(
        form.get("grant_type").map(String::as_str),
        Some("refresh_token")
    );
    assert_eq!(
        form.get("refresh_token").map(String::as_str),
        Some("persisted-refresh")
    );
    assert_eq!(
        form.get("client_id").map(String::as_str),
        Some("refresh-client")
    );
    assert_eq!(
        form.get("client_secret").map(String::as_str),
        Some("refresh-secret")
    );
}

fn form_body(body: &[u8]) -> BTreeMap<String, String> {
    url::form_urlencoded::parse(body).into_owned().collect()
}
