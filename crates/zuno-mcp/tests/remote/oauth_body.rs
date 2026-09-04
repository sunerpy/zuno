//! Every OAuth body a remote MCP peer can answer with is bounded, and what an
//! oversized one *means* depends on who chose the URL.
//!
//! `client.rs` enters the flow automatically on any `401` unless `oauth: false`, and
//! `discovery.rs` takes the first metadata URL verbatim out of the peer's own
//! `WWW-Authenticate` challenge — there the peer chose both the address and the size,
//! so a body past the bound ends the flow naming the bound. The `.well-known` paths are
//! URLs *this client* guessed; an oversized page there is skipped like any other
//! unusable candidate, because a catch-all route answering 200 with a portal page is at
//! least as likely as an attack and a hard login failure may not key on a number the
//! peer picked. Skipping still cannot let login proceed on an unread document.
//!
//! Every fixture that must be refused is a **valid** document padded past the bound, so
//! a test that passes cannot be passing because the parse failed anyway — and the same
//! shape is what proves a skip: a valid oversized document that login never reaches was
//! kept out by the bound and nothing else. The one `text/html` fixture is deliberately
//! not JSON, because a catch-all route is the case it stands for.

use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};
use zuno_auth::{ClientInfo, Entry, McpAuthStore, Secret, Tokens};
use zuno_mcp::{MAX_OAUTH_BODY_BYTES, RemoteClient, RemoteConnect, RemoteError};

use crate::remote_support::{self, remote_config};

/// A valid JSON document padded with trailing whitespace past `MAX_OAUTH_BODY_BYTES`.
///
/// Trailing whitespace is ignored by `serde_json`, so the only thing that can
/// refuse this body is a byte bound.
fn padded_past_the_bound(document: &Value) -> Vec<u8> {
    let mut body = serde_json::to_vec(document).expect("serialize OAuth document");
    assert!(
        body.len() < MAX_OAUTH_BODY_BYTES,
        "the fixture document must be small enough that only the padding crosses the bound"
    );
    body.resize(MAX_OAUTH_BODY_BYTES + 1, b' ');
    body
}

fn oversized(document: &Value) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", "application/json")
        .set_body_bytes(padded_past_the_bound(document))
}

fn assert_names_the_bound(error: &RemoteError, what: &str) {
    let RemoteError::OAuth { message, .. } = error else {
        panic!("an OAuth body past the bound must fail closed as an OAuth error: {error:?}")
    };
    assert!(
        message.contains(what)
            && message.contains(&MAX_OAUTH_BODY_BYTES.to_string())
            && message.contains("past the"),
        "the refusal must name what it refused and the bound it enforced: {message}"
    );
}

/// The reviewer's input: a `401` whose `resource_metadata` parameter points the
/// client at an endpoint the same peer controls.
#[tokio::test]
async fn oauth_protected_resource_metadata_body_is_refused_past_the_bound() {
    let server = MockServer::start().await;
    let huge = format!("{}/huge", server.uri());
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .respond_with(ResponseTemplate::new(401).insert_header(
            "www-authenticate",
            format!("Bearer realm=\"x\", resource_metadata=\"{huge}\""),
        ))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/huge"))
        .respond_with(oversized(&json!({
            "resource": format!("{}/mcp", server.uri()),
            "authorization_servers": [format!("{}/auth", server.uri())],
            "scopes_supported": ["tools.read"],
        })))
        .mount(&server)
        .await;
    // Mounted so that an unbounded read reaches a *successful* authorization
    // request: the test then fails for the missing bound and nothing else.
    remote_support::mount_oauth_discovery(&server).await;

    let dir = tempfile::tempdir().expect("auth tempdir");
    let store = McpAuthStore::new(dir.path().join("mcp-auth.json"));
    let config = remote_config(format!("{}/mcp", server.uri()));
    let error = RemoteClient::connect_with_store("resource-metadata", &config, store)
        .await
        .expect_err("a peer-sized protected-resource metadata body must be refused");
    assert_names_the_bound(&error, "protected-resource metadata");
}

/// The control for the test above: the same oversized body at a URL *this client*
/// guessed may not end login.
///
/// `/.well-known/oauth-protected-resource/...` is a path Zuno builds from the configured
/// server URL, not one the peer named. Behind a catch-all — an SPA rewrite, a portal, a
/// proxy error page — it answers 200 with a page that has nothing to do with OAuth, and
/// its size is evidence of nothing: the identical non-JSON page one byte under the bound
/// was already skipped, so failing closed on the byte after it made a hard, user-visible
/// login failure key purely on a number the peer picked. The page here is `text/html`
/// past the bound; discovery must skip it, fall back to the default resource, and reach
/// the authorization request.
#[tokio::test]
async fn an_oversized_body_at_a_client_derived_metadata_path_is_skipped_not_fatal() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/mcp"))
        // No `resource_metadata` parameter: nothing here is a URL the peer chose.
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    for candidate in [
        "/.well-known/oauth-protected-resource/mcp",
        "/.well-known/oauth-protected-resource",
    ] {
        Mock::given(method("GET"))
            .and(path(candidate))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/html")
                    .set_body_bytes(vec![b'x'; MAX_OAUTH_BODY_BYTES + 1]),
            )
            .mount(&server)
            .await;
    }
    Mock::given(method("GET"))
        .and(path("/.well-known/oauth-authorization-server"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "issuer": server.uri(),
            "authorization_endpoint": format!("{}/authorize", server.uri()),
            "token_endpoint": format!("{}/token", server.uri()),
            "registration_endpoint": format!("{}/register", server.uri()),
            "response_types_supported": ["code"],
            "code_challenge_methods_supported": ["S256"],
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/register"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "client_id": "dynamic-client",
        })))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().expect("auth tempdir");
    let store = McpAuthStore::new(dir.path().join("mcp-auth.json"));
    let config = remote_config(format!("{}/mcp", server.uri()));
    let outcome = RemoteClient::connect_with_store("derived-oversize", &config, store)
        .await
        .expect("an oversized page on a guessed path may not end login");
    let RemoteConnect::AuthorizationRequired(request) = outcome else {
        panic!("a 401 must still produce an authorization request")
    };
    assert!(
        request
            .authorization_url()
            .starts_with(&format!("{}/authorize", server.uri())),
        "login must proceed against the discovered authorization server: {}",
        request.authorization_url()
    );
}

/// An authorization-server metadata body past the bound is never parsed, and login
/// cannot proceed on it.
///
/// The document mounted here is **valid** and padded past the bound, so the only thing
/// that can keep it out of the flow is the byte bound: without one it would be read,
/// parsed, and login would continue to its `authorization_endpoint`. It is skipped
/// instead — this path is always a `.well-known` URL *this client* guessed, so an
/// oversized page there is as likely to be a catch-all as an attack — and with no other
/// candidate answering, discovery fails closed.
#[tokio::test]
async fn oauth_authorization_server_metadata_body_past_the_bound_is_never_parsed() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/.well-known/oauth-protected-resource/mcp"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "resource": format!("{}/mcp", server.uri()),
            "authorization_servers": [server.uri()],
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/.well-known/oauth-authorization-server"))
        .respond_with(oversized(&json!({
            "issuer": server.uri(),
            "authorization_endpoint": format!("{}/authorize", server.uri()),
            "token_endpoint": format!("{}/token", server.uri()),
            "registration_endpoint": format!("{}/register", server.uri()),
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/register"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "client_id": "dynamic-client",
        })))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().expect("auth tempdir");
    let store = McpAuthStore::new(dir.path().join("mcp-auth.json"));
    let config = remote_config(format!("{}/mcp", server.uri()));
    let error = RemoteClient::connect_with_store("as-metadata", &config, store)
        .await
        .expect_err("an authorization-server metadata body past the bound may not be used");
    let RemoteError::OAuth { message, .. } = &error else {
        panic!("no usable authorization metadata must fail closed as an OAuth error: {error:?}")
    };
    assert_eq!(
        message, "authorization server metadata discovery failed",
        "login may not proceed on a document that was never read"
    );
}

/// And the skip is what lets a working server survive a catch-all on the first path.
///
/// Both `.well-known` URLs are client guesses. A portal, SPA rewrite, or proxy error
/// page at the first one used to end login permanently; the real OIDC document at the
/// second one is now reached. The first document is valid and oversized, the second is
/// valid and small, and they name *different* authorization endpoints — so which
/// endpoint the flow arrives at proves which body was read.
#[tokio::test]
async fn an_oversized_first_authorization_candidate_does_not_hide_the_second() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/.well-known/oauth-protected-resource/mcp"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "resource": format!("{}/mcp", server.uri()),
            "authorization_servers": [server.uri()],
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/.well-known/oauth-authorization-server"))
        .respond_with(oversized(&json!({
            "issuer": server.uri(),
            "authorization_endpoint": format!("{}/catch-all-authorize", server.uri()),
            "token_endpoint": format!("{}/token", server.uri()),
            "registration_endpoint": format!("{}/register", server.uri()),
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/.well-known/openid-configuration"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "issuer": server.uri(),
            "authorization_endpoint": format!("{}/oidc-authorize", server.uri()),
            "token_endpoint": format!("{}/token", server.uri()),
            "registration_endpoint": format!("{}/register", server.uri()),
            "response_types_supported": ["code"],
            "code_challenge_methods_supported": ["S256"],
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/register"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "client_id": "dynamic-client",
        })))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().expect("auth tempdir");
    let store = McpAuthStore::new(dir.path().join("mcp-auth.json"));
    let config = remote_config(format!("{}/mcp", server.uri()));
    let outcome = RemoteClient::connect_with_store("as-fallback", &config, store)
        .await
        .expect("the second candidate carries usable authorization metadata");
    let RemoteConnect::AuthorizationRequired(request) = outcome else {
        panic!("a 401 must still produce an authorization request")
    };
    let url = request.authorization_url();
    assert!(
        url.starts_with(&format!("{}/oidc-authorize", server.uri())),
        "the oversized first candidate may neither be parsed nor end the search: {url}"
    );
}

#[tokio::test]
async fn oauth_dynamic_registration_body_is_refused_past_the_bound() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    remote_support::mount_default_oauth_discovery(&server).await;
    Mock::given(method("POST"))
        .and(path("/register"))
        .respond_with(oversized(&json!({
            "client_id": "dynamic-client",
            "client_secret": "dynamic-secret",
        })))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().expect("auth tempdir");
    let store = McpAuthStore::new(dir.path().join("mcp-auth.json"));
    let config = remote_config(format!("{}/mcp", server.uri()));
    let error = RemoteClient::connect_with_store("registration", &config, store)
        .await
        .expect_err("a peer-sized dynamic registration body must be refused");
    assert_names_the_bound(&error, "dynamic client registration");
}

#[tokio::test]
async fn oauth_token_body_is_refused_past_the_bound() {
    let server = MockServer::start().await;
    let url = format!("{}/mcp", server.uri());
    remote_support::mount_default_oauth_discovery(&server).await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(oversized(&json!({
            "access_token": "fresh-access",
            "expires_in": 3600,
        })))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().expect("auth tempdir");
    let store = McpAuthStore::new(dir.path().join("mcp-auth.json"));
    store
        .set(
            "token",
            Entry {
                tokens: Some(Tokens {
                    access_token: Secret::new("expired-access"),
                    refresh_token: Some(Secret::new("persisted-refresh")),
                    expires_at: Some(1),
                    scope: None,
                }),
                client_info: Some(ClientInfo {
                    client_id: "refresh-client".to_owned(),
                    client_secret: None,
                    client_id_issued_at: None,
                    client_secret_expires_at: Some(0),
                }),
                ..Entry::default()
            },
            Some(&url),
        )
        .expect("seed expired credentials");
    let error = RemoteClient::connect_with_store("token", &remote_config(url), store)
        .await
        .expect_err("a peer-sized token response must be refused");
    assert_names_the_bound(&error, "token response");
}
