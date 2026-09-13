use serde_json::json;
use wiremock::matchers::{body_string_contains, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};
use zuno_auth::Secret;
use zuno_mcp::{RemoteClient, RemoteError, RemoteTransport};

use crate::remote_support::{initialize_result, remote_config};

#[tokio::test]
async fn explicit_client_keeps_owner_credentials_and_mcp_sessions_separate() {
    let server = MockServer::start().await;
    for owner in ["alice", "bob"] {
        Mock::given(method("POST"))
            .and(path("/mcp"))
            .and(header("authorization", format!("Bearer {owner}")))
            .and(header("x-host-policy", "injected"))
            .and(body_string_contains(r#""method":"initialize""#))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .insert_header("mcp-session-id", owner)
                    .set_body_json(json!({
                        "jsonrpc":"2.0","id":1,
                        "result":initialize_result("2025-03-26", "shared-server")
                    })),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(header("authorization", format!("Bearer {owner}")))
            .and(header("mcp-session-id", owner))
            .and(body_string_contains("notifications/initialized"))
            .respond_with(ResponseTemplate::new(202))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(header("authorization", format!("Bearer {owner}")))
            .and(header("mcp-session-id", owner))
            .and(body_string_contains(r#""method":"tools/call""#))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc":"2.0","id":2,
                "result":{"content":[{"type":"text","text":owner}]}
            })))
            .expect(1)
            .mount(&server)
            .await;
    }
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .default_headers(reqwest::header::HeaderMap::from_iter([(
            reqwest::header::HeaderName::from_static("x-host-policy"),
            reqwest::header::HeaderValue::from_static("injected"),
        )]))
        .build()
        .unwrap();
    let config = remote_config(format!("{}/mcp", server.uri()));
    let alice = RemoteClient::connect_with_client(
        "same-name",
        &config,
        Some(Secret::new("alice")),
        http.clone(),
    )
    .await
    .unwrap();
    let bob =
        RemoteClient::connect_with_client("same-name", &config, Some(Secret::new("bob")), http)
            .await
            .unwrap();
    assert_eq!(alice.transport(), RemoteTransport::StreamableHttp);
    let first = alice
        .call_tool("identity", Default::default())
        .await
        .unwrap();
    let second = bob.call_tool("identity", Default::default()).await.unwrap();
    assert_eq!(first.content[0]["text"], "alice");
    assert_eq!(second.content[0]["text"], "bob");
    alice.close().await;
    bob.close().await;
}

#[tokio::test]
async fn explicit_client_does_not_start_oauth_or_retry_a_different_transport() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .respond_with(ResponseTemplate::new(401).insert_header(
            "www-authenticate",
            "Bearer resource_metadata=\"/oauth-resource\"",
        ))
        .expect(1)
        .mount(&server)
        .await;
    let error = RemoteClient::connect_with_client(
        "no-global-auth",
        &remote_config(format!("{}/mcp", server.uri())),
        None,
        reqwest::Client::new(),
    )
    .await
    .unwrap_err();
    assert!(matches!(
        error,
        RemoteError::Status {
            status: reqwest::StatusCode::UNAUTHORIZED,
            ..
        }
    ));
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn explicit_client_obeys_host_redirect_refusal() {
    let origin = MockServer::start().await;
    let redirected = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(307)
                .insert_header("location", format!("{}/mcp", redirected.uri())),
        )
        .mount(&origin)
        .await;
    let error = RemoteClient::connect_with_client(
        "fixed-target",
        &remote_config(format!("{}/mcp", origin.uri())),
        Some(Secret::new("target-bound")),
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap(),
    )
    .await
    .unwrap_err();
    assert!(matches!(
        error,
        RemoteError::Status {
            status: reqwest::StatusCode::TEMPORARY_REDIRECT,
            ..
        }
    ));
    assert!(redirected.received_requests().await.unwrap().is_empty());
}
