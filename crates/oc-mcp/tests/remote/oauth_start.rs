use std::collections::BTreeMap;
use std::num::NonZeroU16;

use base64::Engine as _;
use oc_auth::McpAuthStore;
use oc_config::schema::mcp::{McpOauth, McpOauthConfig};
use oc_mcp::{RemoteClient, RemoteConnect};
use sha2::{Digest, Sha256};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::remote_support::{self, remote_config};

#[tokio::test]
async fn remote_unauthorized_starts_dynamic_registration_and_pkce_by_default() {
    let server = MockServer::start().await;
    let resource_url = format!("{}/oauth-resource", server.uri());
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .respond_with(ResponseTemplate::new(401).insert_header(
            "www-authenticate",
            format!("Bearer resource_metadata=\"{resource_url}\""),
        ))
        .expect(1)
        .mount(&server)
        .await;
    remote_support::mount_oauth_discovery(&server).await;
    let dir = tempfile::tempdir().expect("auth tempdir");
    let store = McpAuthStore::new(dir.path().join("mcp-auth.json"));
    let mut config = remote_config(format!("{}/mcp", server.uri()));
    config.oauth = Some(McpOauth::Config(McpOauthConfig {
        scope: Some("tools.read".to_owned()),
        callback_port: NonZeroU16::new(31_987),
        ..McpOauthConfig::default()
    }));
    let outcome = RemoteClient::connect_with_store("oauth", &config, store.clone())
        .await
        .expect("401 starts OAuth instead of becoming a transport failure");
    let RemoteConnect::AuthorizationRequired(request) = outcome else {
        panic!("the OAuth flow must pause for browser authorization")
    };
    let authorization = url::Url::parse(request.authorization_url()).expect("authorization URL");
    let query: BTreeMap<_, _> = authorization.query_pairs().into_owned().collect();
    assert_eq!(authorization.path(), "/authorize");
    assert_eq!(
        query.get("client_id").map(String::as_str),
        Some("dynamic-client")
    );
    assert_eq!(query.get("scope").map(String::as_str), Some("tools.read"));
    assert_eq!(
        query.get("code_challenge_method").map(String::as_str),
        Some("S256")
    );
    assert_eq!(
        query.get("redirect_uri").map(String::as_str),
        Some("http://127.0.0.1:31987/mcp/oauth/callback")
    );
    let stored = store
        .get("oauth")
        .expect("read auth store")
        .expect("OAuth state persisted");
    assert_eq!(
        stored
            .client_info
            .as_ref()
            .map(|info| info.client_id.as_str()),
        Some("dynamic-client")
    );
    let verifier = stored.code_verifier.as_ref().expect("PKCE verifier");
    let expected_challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(Sha256::digest(verifier.expose().as_bytes()));
    assert_eq!(query.get("code_challenge"), Some(&expected_challenge));
    let state = query.get("state").expect("OAuth state");
    let rendered = format!("{request:?} {stored:?}");
    assert!(!rendered.contains("dynamic-secret"));
    assert!(!rendered.contains(state));
    remote_support::assert_private_mode(store.path());
}
