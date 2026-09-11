use super::*;
use crate::login::*;
use sha2::{Digest, Sha256};

struct Exchange {
    calls: AtomicUsize,
    expected: Mutex<Option<(String, String)>>,
    fail: bool,
    access_client: String,
}

#[async_trait]
impl AuthorizationCodeExchange for Exchange {
    async fn exchange(
        &self,
        _endpoint: &reqwest::Url,
        request: CodeRequest<'_>,
    ) -> Result<LoginTokens, LoginError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(request.client_id, CLIENT);
        assert_eq!(
            request.redirect_uri.as_str(),
            "https://agent.example/auth/callback"
        );
        assert_eq!(request.code, "authorized-code");
        let (nonce, challenge) = self.expected.lock().unwrap().clone().unwrap();
        assert_eq!(
            URL_SAFE_NO_PAD.encode(Sha256::digest(request.verifier.as_bytes())),
            challenge
        );
        if self.fail {
            return Err(LoginError::Exchange);
        }
        let mut access_claims = claims();
        access_claims["azp"] = json!(self.access_client);
        let access = signed(0, &access_claims);
        let hash = Sha256::digest(access.as_bytes());
        let now = jsonwebtoken::get_current_timestamp();
        let id = signed(
            0,
            &json!({
                "iss":config().issuer(),"sub":"login-subject","aud":CLIENT,"nonce":nonce,
                "iat":now-1,"exp":now+300,
                "at_hash":URL_SAFE_NO_PAD.encode(&hash[..hash.len()/2]),
            }),
        );
        Ok(LoginTokens {
            access_token: zuno_auth::Secret::new(access),
            id_token: zuno_auth::Secret::new(id),
        })
    }
}

fn login_config() -> OidcLoginConfig {
    login_config_with_options(OidcLoginOptions::default())
}

fn login_config_with_options(options: OidcLoginOptions) -> OidcLoginConfig {
    let issuer = config().issuer();
    OidcLoginConfig::from_metadata_with_options(
        config().authority(),
        CLIENT.to_owned(),
        "https://agent.example/auth/callback",
        ["openid".to_owned(), "Session.Access".to_owned()].into(),
        &serde_json::to_vec(&json!({
            "issuer":issuer,"authorization_endpoint":format!("{issuer}/authorize"),
            "token_endpoint":format!("{issuer}/token"),"response_types_supported":["code"],
        }))
        .unwrap(),
        options,
    )
    .unwrap()
}

fn client(fail: bool) -> (OidcLoginClient, Arc<Exchange>) {
    let (access, keys) = verifier_with(config(), Source::new());
    let exchange = Arc::new(Exchange {
        calls: AtomicUsize::new(0),
        expected: Mutex::new(None),
        fail,
        access_client: CLIENT.to_owned(),
    });
    (
        OidcLoginClient::new(login_config(), keys, exchange.clone(), access).unwrap(),
        exchange,
    )
}

#[tokio::test]
async fn code_login_rejects_an_access_token_issued_to_another_allowed_client() {
    let other = "dddddddd-dddd-4ddd-8ddd-dddddddddddd";
    let mut policy = config();
    policy.allowed_clients.insert(other.to_owned());
    let (access, keys) = verifier_with(policy, Source::new());
    let exchange = Arc::new(Exchange {
        calls: AtomicUsize::new(0),
        expected: Mutex::new(None),
        fail: false,
        access_client: other.to_owned(),
    });
    let client = OidcLoginClient::new(login_config(), keys, exchange.clone(), access).unwrap();
    let login = prepare(&client, &exchange);
    let state = login.attempt.state().to_owned();
    assert!(
        matches!(
            client
                .complete(
                    login.attempt,
                    &state,
                    &"browser-binding-".repeat(4),
                    "authorized-code"
                )
                .await,
            Err(LoginError::Transaction),
        ),
        "a shared API allowlist must not let another client create this BFF's session"
    );
    assert_eq!(exchange.calls.load(Ordering::SeqCst), 1);
}

fn prepare(client: &OidcLoginClient, exchange: &Exchange) -> LoginStart {
    let login = client.begin(&"browser-binding-".repeat(4)).unwrap();
    let fields = login
        .authorization_url
        .query_pairs()
        .into_owned()
        .collect::<std::collections::BTreeMap<_, _>>();
    assert_eq!(fields["response_type"], "code");
    assert_eq!(fields["code_challenge_method"], "S256");
    assert_eq!(fields["state"], login.attempt.state());
    assert!(!fields.contains_key("code_verifier"));
    *exchange.expected.lock().unwrap() =
        Some((fields["nonce"].clone(), fields["code_challenge"].clone()));
    login
}

#[tokio::test]
async fn code_login_binds_pkce_nonce_and_resource_identity_without_returning_tokens() {
    let (client, exchange) = client(false);
    let login = prepare(&client, &exchange);
    let state = login.attempt.state().to_owned();
    let result = client
        .complete(
            login.attempt,
            &state,
            &"browser-binding-".repeat(4),
            "authorized-code",
        )
        .await
        .unwrap();
    assert_eq!(result.identity.tenant_id().as_str(), TENANT);
    assert_eq!(result.identity.principal_id().as_str(), SUBJECT);
    assert_eq!(result.identity.issuer(), config().issuer());
    assert_eq!(exchange.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn mismatched_browser_or_state_cannot_redeem_a_code_and_exchange_failure_is_not_retried() {
    let (client, exchange) = client(true);
    let login = prepare(&client, &exchange);
    let state = login.attempt.state().to_owned();
    assert!(
        client
            .complete(login.attempt, &state, "another-browser", "authorized-code")
            .await
            .is_err()
    );
    assert_eq!(exchange.calls.load(Ordering::SeqCst), 0);
    let login = prepare(&client, &exchange);
    assert!(
        client
            .complete(
                login.attempt,
                "another-state",
                &"browser-binding-".repeat(4),
                "authorized-code"
            )
            .await
            .is_err()
    );
    assert_eq!(exchange.calls.load(Ordering::SeqCst), 0);
    let login = prepare(&client, &exchange);
    let state = login.attempt.state().to_owned();
    assert!(
        client
            .complete(
                login.attempt,
                &state,
                &"browser-binding-".repeat(4),
                "authorized-code"
            )
            .await
            .is_err()
    );
    assert_eq!(exchange.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn provider_metadata_cannot_change_issuer_or_send_client_credentials_to_another_origin() {
    for (issuer, endpoint) in [
        (
            "https://other.example".to_owned(),
            format!("{}/token", config().issuer()),
        ),
        (config().issuer(), "https://other.example/token".to_owned()),
        (
            config().issuer(),
            format!("{}/token?client_id=other", config().issuer()),
        ),
    ] {
        assert!(OidcLoginConfig::from_metadata(
            config().authority(), CLIENT.to_owned(), "https://agent.example/auth/callback",
            ["openid".to_owned()].into(),
            &serde_json::to_vec(&json!({
                "issuer":issuer,"authorization_endpoint":format!("{}/authorize",config().issuer()),
                "token_endpoint":endpoint,"response_types_supported":["code"],
            })).unwrap(),
        ).is_err());
    }
}

#[tokio::test]
async fn stored_login_is_authenticated_encrypted_and_bound_to_its_browser_and_expiry() {
    use crate::login_state::LoginStateCipher;
    let (client, exchange) = client(false);
    let start = prepare(&client, &exchange);
    let state = start.attempt.state().to_owned();
    let cipher = LoginStateCipher::new(
        "current".to_owned(),
        [("current".to_owned(), vec![7; 32])].into(),
    )
    .unwrap();
    let encrypted = cipher.seal(start.attempt).unwrap();
    assert!(!String::from_utf8_lossy(&encrypted.ciphertext).contains(&state));
    let binding = "browser-binding-".repeat(4);
    let now = jsonwebtoken::get_current_timestamp();
    let mut tampered = encrypted.clone();
    tampered.expires_at += 1;
    assert!(cipher.open(tampered, &state, &binding, now).is_err());
    let mut tampered = encrypted.clone();
    tampered.ciphertext[0] ^= 1;
    assert!(cipher.open(tampered, &state, &binding, now).is_err());
    assert!(
        cipher
            .open(encrypted.clone(), &state, "another-browser", now)
            .is_err()
    );
    assert!(
        cipher
            .open(encrypted.clone(), &state, &binding, encrypted.expires_at)
            .is_err()
    );
    let rotated = LoginStateCipher::new(
        "next".to_owned(),
        [
            ("current".to_owned(), vec![7; 32]),
            ("next".to_owned(), vec![9; 32]),
        ]
        .into(),
    )
    .unwrap();
    let retired =
        LoginStateCipher::new("next".to_owned(), [("next".to_owned(), vec![9; 32])].into())
            .unwrap();
    assert!(
        retired
            .open(encrypted.clone(), &state, &binding, now)
            .is_err()
    );
    let attempt = rotated.open(encrypted, &state, &binding, now).unwrap();
    client
        .complete(attempt, &state, &binding, "authorized-code")
        .await
        .unwrap();
}

#[tokio::test]
async fn login_policy_caps_the_session_and_invalidates_transactions_after_config_changes() {
    let (access, keys) = verifier_with(config(), Source::new());
    let config = login_config_with_options(
        serde_json::from_value(json!({
            "transactionLifetimeSeconds":60,"sessionLifetimeSeconds":60,
        }))
        .unwrap(),
    );
    let exchange = Arc::new(Exchange {
        calls: AtomicUsize::new(0),
        expected: Mutex::new(None),
        fail: false,
        access_client: CLIENT.to_owned(),
    });
    let before = jsonwebtoken::get_current_timestamp();
    let client = OidcLoginClient::new(
        config.clone(),
        keys.clone(),
        exchange.clone(),
        access.clone(),
    )
    .unwrap();
    let start = prepare(&client, &exchange);
    assert!(
        (before + 60..=jsonwebtoken::get_current_timestamp() + 60)
            .contains(&start.attempt.expires_at_seconds())
    );
    let state = start.attempt.state().to_owned();
    let result = client
        .complete(
            start.attempt,
            &state,
            &"browser-binding-".repeat(4),
            "authorized-code",
        )
        .await
        .unwrap();
    assert!(result.expires_at_seconds <= jsonwebtoken::get_current_timestamp() + 60);
    let start = prepare(&client, &exchange);
    let state = start.attempt.state().to_owned();
    let changed_config = login_config_with_options(
        serde_json::from_value(json!({"sessionLifetimeSeconds":120})).unwrap(),
    );
    let changed = OidcLoginClient::new(changed_config, keys, exchange.clone(), access).unwrap();
    assert!(
        changed
            .complete(
                start.attempt,
                &state,
                &"browser-binding-".repeat(4),
                "authorized-code"
            )
            .await
            .is_err()
    );
    assert_eq!(
        exchange.calls.load(Ordering::SeqCst),
        1,
        "config drift is rejected before any token exchange"
    );
}

#[test]
fn generic_login_endpoint_origins_are_explicit_and_lifetimes_are_bounded() {
    for value in [
        json!({"transactionLifetimeSeconds":0}),
        json!({"transactionLifetimeSeconds":601}),
        json!({"sessionLifetimeSeconds":86401}),
        json!({"clockSkewSeconds":121}),
        json!({"maxAuthenticationAgeSeconds":0}),
        json!({"additionalEndpointOrigins":["http://identity.example"]}),
        json!({"additionalEndpointOrigins":["https://identity.example/private"]}),
    ] {
        assert!(serde_json::from_value::<OidcLoginOptions>(value).is_err());
    }
    let options = serde_json::from_value(json!({
        "additionalEndpointOrigins":["https://tokens.example"],
        "maxAuthenticationAgeSeconds":120,
    }))
    .unwrap();
    let authority = config().authority();
    let metadata = serde_json::to_vec(&json!({
        "issuer":authority.issuer(),"authorization_endpoint":format!("{}/authorize",authority.issuer()),
        "token_endpoint":"https://tokens.example/oauth/token","response_types_supported":["code"],
    })).unwrap();
    assert!(
        OidcLoginConfig::from_metadata(
            authority.clone(),
            CLIENT.to_owned(),
            "https://agent.example/auth/callback",
            ["openid".to_owned()].into(),
            &metadata,
        )
        .is_err()
    );
    OidcLoginConfig::from_metadata_with_options(
        authority,
        CLIENT.to_owned(),
        "https://agent.example/auth/callback",
        ["openid".to_owned()].into(),
        &metadata,
        options,
    )
    .unwrap();
}
