use std::path::Path;

use oc_config::schema::mcp::{McpRemote, RemoteKind};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

pub fn remote_config(url: String) -> McpRemote {
    McpRemote {
        kind: RemoteKind::Remote,
        url,
        enabled: None,
        headers: None,
        oauth: None,
        timeout: None,
    }
}

pub fn initialize_result(protocol_version: &str, server: &str) -> Value {
    json!({
        "protocolVersion": protocol_version,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": server, "version": "1.0.0" },
    })
}

pub fn sse_event(kind: &str, data: &str) -> String {
    format!("event: {kind}\ndata: {data}\n\n")
}

pub async fn mount_oauth_discovery(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/oauth-resource"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "resource": format!("{}/mcp", server.uri()),
            "authorization_servers": [format!("{}/auth", server.uri())],
            "scopes_supported": ["tools.read"],
        })))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/.well-known/oauth-authorization-server/auth"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "issuer": format!("{}/auth", server.uri()),
            "authorization_endpoint": format!("{}/authorize", server.uri()),
            "token_endpoint": format!("{}/token", server.uri()),
            "registration_endpoint": format!("{}/register", server.uri()),
            "response_types_supported": ["code"],
            "code_challenge_methods_supported": ["S256"],
            "token_endpoint_auth_methods_supported": ["none", "client_secret_post"],
        })))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/register"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "client_id": "dynamic-client",
            "client_secret": "dynamic-secret",
            "client_id_issued_at": 1_700_000_000,
            "client_secret_expires_at": 0,
        })))
        .mount(server)
        .await;
}

pub async fn mount_default_oauth_discovery(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/.well-known/oauth-protected-resource/mcp"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "resource": format!("{}/mcp", server.uri()),
            "authorization_servers": [server.uri()],
            "scopes_supported": ["tools.read"],
        })))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/.well-known/oauth-authorization-server"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "issuer": server.uri(),
            "authorization_endpoint": format!("{}/authorize", server.uri()),
            "token_endpoint": format!("{}/token", server.uri()),
            "registration_endpoint": format!("{}/register", server.uri()),
            "response_types_supported": ["code"],
            "code_challenge_methods_supported": ["S256"],
            "token_endpoint_auth_methods_supported": ["none", "client_secret_post"],
        })))
        .mount(server)
        .await;
}

pub fn assert_private_mode(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path)
            .expect("auth store metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }
    #[cfg(not(unix))]
    let _ = path;
}
