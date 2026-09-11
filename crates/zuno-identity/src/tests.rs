use std::collections::BTreeSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use aws_lc_rs::rsa::{KeyPair, KeySize};
use aws_lc_rs::signature::KeyPair as _;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use serde_json::{Value, json};

use crate::*;

const TENANT: &str = "11111111-1111-4111-8111-111111111111";
const AUDIENCE: &str = "22222222-2222-4222-8222-222222222222";
const CLIENT: &str = "33333333-3333-4333-8333-333333333333";
const SUBJECT: &str = "44444444-4444-4444-8444-444444444444";
const OTHER: &str = "55555555-5555-4555-8555-555555555555";

struct TestKey {
    key: KeyPair,
    n: String,
    e: String,
}

fn test_keys() -> &'static [TestKey; 2] {
    static KEYS: OnceLock<[TestKey; 2]> = OnceLock::new();
    KEYS.get_or_init(|| {
        std::array::from_fn(|_| {
            let rsa = KeyPair::generate(KeySize::Rsa2048).unwrap();
            let public =
                aws_lc_rs::signature::RsaPublicKeyComponents::<Vec<u8>>::from(rsa.public_key());
            let (n, e) = (public.n, public.e);
            TestKey {
                key: rsa,
                n: URL_SAFE_NO_PAD.encode(n),
                e: URL_SAFE_NO_PAD.encode(e),
            }
        })
    })
}

fn config() -> EntraConfig {
    EntraConfig::new(
        TENANT,
        AUDIENCE,
        [CLIENT.to_owned()].into(),
        ActorPolicy::DelegatedUser {
            required_scopes: ["Session.Access".to_owned()].into(),
            required_roles: BTreeSet::new(),
        },
    )
    .unwrap()
}

fn claims() -> Value {
    let now = jsonwebtoken::get_current_timestamp();
    json!({
        "iss": config().issuer(),
        "aud": AUDIENCE,
        "tid": TENANT,
        "oid": SUBJECT,
        "azp": CLIENT,
        "sub": "pairwise-subject-not-the-resource-owner",
        "ver": "2.0",
        "exp": now + 3600,
        "nbf": now - 30,
        "iat": now - 30,
        "scp": "Session.Access",
        "preferred_username": "not-an-authorization-key@example.test"
    })
}

fn key_entry(index: usize) -> Value {
    json!({
        "kid": format!("test-key-{index}"),
        "kty": "RSA",
        "use": "sig",
        "issuer": "https://login.microsoftonline.com/{tenantid}/v2.0",
        "n": test_keys()[index].n,
        "e": test_keys()[index].e
    })
}

fn key_document(indices: &[usize]) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "keys": indices.iter().map(|index| key_entry(*index)).collect::<Vec<_>>()
    }))
    .unwrap()
}

fn signed(index: usize, claims: &Value) -> String {
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some(format!("test-key-{index}"));
    sign_with_header(index, &header, claims)
}

fn sign_with_header(index: usize, header: &Header, claims: &Value) -> String {
    let input = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(header).unwrap()),
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims).unwrap())
    );
    let key = &test_keys()[index].key;
    let mut signature = vec![0; key.public_modulus_len()];
    key.sign(
        &aws_lc_rs::signature::RSA_PKCS1_SHA256,
        &aws_lc_rs::rand::SystemRandom::new(),
        input.as_bytes(),
        &mut signature,
    )
    .unwrap();
    format!("{input}.{}", URL_SAFE_NO_PAD.encode(signature))
}

struct Source {
    document: Mutex<Result<Vec<u8>, IdentityError>>,
    calls: AtomicUsize,
    delay: Duration,
}

impl Source {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            document: Mutex::new(Ok(key_document(&[0]))),
            calls: AtomicUsize::new(0),
            delay: Duration::ZERO,
        })
    }
}

#[async_trait]
impl SigningKeySource for Source {
    async fn fetch(&self) -> Result<Vec<u8>, IdentityError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        self.document.lock().unwrap().clone()
    }
}

fn verifier_with(
    config: EntraConfig,
    source: Arc<Source>,
) -> (Arc<EntraVerifier>, Arc<SigningKeyCache>) {
    let cache = Arc::new(
        SigningKeyCache::new(
            config.authority(),
            source,
            KeyCacheOptions {
                max_age: Duration::from_secs(120),
                refresh_interval: Duration::from_secs(10),
            },
        )
        .unwrap(),
    );
    (
        Arc::new(EntraVerifier::with_keys(config, cache.clone()).unwrap()),
        cache,
    )
}

#[tokio::test]
async fn verified_owner_uses_tenant_object_id_and_host_policy_revision() {
    let source = Source::new();
    let (verifier, _) = verifier_with(config(), source.clone());
    let token = signed(0, &claims());
    let identity = verifier.verify(&token).await.unwrap();
    assert_eq!(identity.tenant_id().as_str(), TENANT);
    assert_eq!(identity.principal_id().as_str(), SUBJECT);
    assert_eq!(identity.client_id().as_str(), CLIENT);
    assert_eq!(identity.kind(), VerifiedIdentityKind::DelegatedUser);
    let scope = identity.attribution(9.try_into().unwrap());
    assert_eq!(scope.policy_revision().get(), 9);
    assert_eq!(scope.owner().principal_id.as_str(), SUBJECT);
    let mut changed_aliases = claims();
    changed_aliases["preferred_username"] = json!("someone-else@example.test");
    changed_aliases["sub"] = json!("another-client-pairwise-subject");
    changed_aliases["policyRevision"] = json!(999);
    assert_eq!(
        verifier
            .verify(&signed(0, &changed_aliases))
            .await
            .unwrap()
            .attribution(9.try_into().unwrap()),
        scope
    );
    assert_eq!(source.calls.load(Ordering::SeqCst), 1);
    assert!(!format!("{identity:?}").contains(&token));
    assert!(!format!("{identity:?}").contains("preferred_username"));
}

#[tokio::test]
async fn signature_audience_tenant_client_time_and_scopes_are_required() {
    let (verifier, _) = verifier_with(config(), Source::new());
    let now = jsonwebtoken::get_current_timestamp();
    for (field, value, rejection) in [
        ("aud", json!(OTHER), TokenRejection::Audience),
        (
            "iss",
            json!("https://attacker.test"),
            TokenRejection::Issuer,
        ),
        ("tid", json!(OTHER), TokenRejection::Claims),
        ("oid", json!("user@example.test"), TokenRejection::Claims),
        ("azp", json!(OTHER), TokenRejection::Client),
        ("ver", json!("1.0"), TokenRejection::Claims),
        ("exp", json!(now - 300), TokenRejection::Expired),
        ("nbf", json!(now + 300), TokenRejection::NotYetValid),
        ("iat", json!(now + 300), TokenRejection::Claims),
        ("scp", json!("Other.Access"), TokenRejection::Permissions),
        ("idtyp", json!("app"), TokenRejection::PrincipalKind),
    ] {
        let mut invalid = claims();
        invalid[field] = value;
        assert_eq!(
            verifier.verify(&signed(0, &invalid)).await.unwrap_err(),
            IdentityError::Rejected(rejection),
            "{field}"
        );
    }
    for missing in [
        "exp", "nbf", "iat", "iss", "aud", "sub", "oid", "tid", "azp",
    ] {
        let mut invalid = claims();
        invalid.as_object_mut().unwrap().remove(missing);
        assert!(
            verifier.verify(&signed(0, &invalid)).await.is_err(),
            "{missing}"
        );
    }
    let mut forged_header = Header::new(Algorithm::RS256);
    forged_header.kid = Some("test-key-0".to_owned());
    let forged = sign_with_header(1, &forged_header, &claims());
    assert_eq!(
        verifier.verify(&forged).await.unwrap_err(),
        TokenRejection::InvalidToken.into()
    );
}

#[tokio::test]
async fn id_tokens_user_tokens_and_app_only_tokens_are_not_interchangeable() {
    let source = Source::new();
    let (user, cache) = verifier_with(config(), source);
    let workload_config = EntraConfig::new(
        TENANT,
        AUDIENCE,
        [CLIENT.to_owned()].into(),
        ActorPolicy::Workload {
            required_roles: ["Worker.Run".to_owned()].into(),
        },
    )
    .unwrap();
    let workload = EntraVerifier::with_keys(workload_config, cache).unwrap();
    let mut id_token = claims();
    id_token.as_object_mut().unwrap().remove("scp");
    id_token["roles"] = json!(["Session.Access", "Worker.Run"]);
    assert!(user.verify(&signed(0, &id_token)).await.is_err());
    assert!(workload.verify(&signed(0, &id_token)).await.is_err());
    assert!(workload.verify(&signed(0, &claims())).await.is_err());

    let mut app_token = id_token;
    app_token["idtyp"] = json!("app");
    let identity = workload.verify(&signed(0, &app_token)).await.unwrap();
    assert_eq!(identity.kind(), VerifiedIdentityKind::Workload);
    assert_eq!(
        identity.attribution(1.try_into().unwrap()).kind(),
        zuno_types::identity::PrincipalKind::Workload
    );
    assert!(user.verify(&signed(0, &app_token)).await.is_err());
    app_token["roles"] = json!(["Other.Run"]);
    assert_eq!(
        workload.verify(&signed(0, &app_token)).await.unwrap_err(),
        TokenRejection::Permissions.into()
    );
}

#[tokio::test]
async fn untrusted_headers_and_oversized_tokens_do_not_fetch_keys() {
    let source = Source::new();
    let (verifier, _) = verifier_with(config(), source.clone());
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some("test-key-0".to_owned());
    header.jku = Some("http://169.254.169.254/private-key".to_owned());
    let token = sign_with_header(0, &header, &claims());
    assert_eq!(
        verifier.verify(&token).await.unwrap_err(),
        TokenRejection::UnsupportedToken.into()
    );
    header.jku = None;
    header.crit = Some(vec!["future-extension".to_owned()]);
    let token = sign_with_header(0, &header, &claims());
    assert!(verifier.verify(&token).await.is_err());
    let hs_token = encode(
        &Header::new(Algorithm::HS256),
        &claims(),
        &EncodingKey::from_secret(b"not-a-trusted-key"),
    )
    .unwrap();
    assert!(verifier.verify(&hs_token).await.is_err());
    assert!(verifier.verify(&"x".repeat(32769)).await.is_err());
    assert!(verifier.verify("Bearer token").await.is_err());
    assert_eq!(source.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn key_rotation_is_single_flight_throttled_and_expiry_fails_closed() {
    let source = Arc::new(Source {
        document: Mutex::new(Ok(key_document(&[0]))),
        calls: AtomicUsize::new(0),
        delay: Duration::from_millis(1),
    });
    let (verifier, _) = verifier_with(config(), source.clone());
    let mut requests = tokio::task::JoinSet::new();
    for _ in 0..16 {
        let verifier = verifier.clone();
        requests.spawn(async move { verifier.verify(&signed(0, &claims())).await });
    }
    while let Some(result) = requests.join_next().await {
        result.unwrap().unwrap();
    }
    assert_eq!(source.calls.load(Ordering::SeqCst), 1);
    *source.document.lock().unwrap() = Ok(key_document(&[0, 1]));
    for _ in 0..16 {
        assert_eq!(
            verifier.verify(&signed(1, &claims())).await.unwrap_err(),
            TokenRejection::UnknownKey.into()
        );
    }
    assert_eq!(source.calls.load(Ordering::SeqCst), 1);
    tokio::time::advance(Duration::from_secs(11)).await;
    verifier.verify(&signed(1, &claims())).await.unwrap();
    assert_eq!(source.calls.load(Ordering::SeqCst), 2);
    *source.document.lock().unwrap() = Err(IdentityError::KeysUnavailable);
    tokio::time::advance(Duration::from_secs(121)).await;
    for _ in 0..16 {
        assert_eq!(
            verifier.verify(&signed(0, &claims())).await.unwrap_err(),
            IdentityError::KeysUnavailable
        );
    }
    assert_eq!(source.calls.load(Ordering::SeqCst), 3);
}

#[tokio::test(start_paused = true)]
async fn cancelling_a_refresh_does_not_remove_the_backoff() {
    let source = Arc::new(Source {
        document: Mutex::new(Ok(key_document(&[0]))),
        calls: AtomicUsize::new(0),
        delay: Duration::from_secs(5),
    });
    let (verifier, _) = verifier_with(config(), source.clone());
    let task = tokio::spawn({
        let verifier = verifier.clone();
        async move { verifier.verify(&signed(0, &claims())).await }
    });
    tokio::task::yield_now().await;
    assert_eq!(source.calls.load(Ordering::SeqCst), 1);
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_eq!(
        verifier.verify(&signed(0, &claims())).await.unwrap_err(),
        IdentityError::KeysUnavailable
    );
    assert_eq!(source.calls.load(Ordering::SeqCst), 1);
    tokio::time::advance(Duration::from_secs(11)).await;
    verifier.verify(&signed(0, &claims())).await.unwrap();
    assert_eq!(source.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test(start_paused = true)]
async fn an_unknown_key_refresh_does_not_block_known_valid_keys() {
    let source = Arc::new(Source {
        document: Mutex::new(Ok(key_document(&[0]))),
        calls: AtomicUsize::new(0),
        delay: Duration::from_secs(5),
    });
    let (verifier, _) = verifier_with(config(), source.clone());
    verifier.verify(&signed(0, &claims())).await.unwrap();
    tokio::time::advance(Duration::from_secs(11)).await;
    let refresh = tokio::spawn({
        let verifier = verifier.clone();
        async move { verifier.verify(&signed(1, &claims())).await }
    });
    tokio::task::yield_now().await;
    assert_eq!(source.calls.load(Ordering::SeqCst), 2);
    tokio::time::timeout(
        Duration::from_millis(10),
        verifier.verify(&signed(0, &claims())),
    )
    .await
    .expect("known valid key must not wait for refresh")
    .unwrap();
    assert_eq!(
        refresh.await.unwrap().unwrap_err(),
        TokenRejection::UnknownKey.into()
    );
}

#[tokio::test]
async fn ambiguous_or_untrusted_key_documents_never_become_a_cache() {
    let source = Source::new();
    let mut wrong_issuer = key_entry(0);
    wrong_issuer["issuer"] = json!(format!("https://login.microsoftonline.com/{OTHER}/v2.0"));
    let mut private = key_entry(0);
    private["d"] = json!("must-not-be-loaded");
    let mut weak = key_entry(0);
    weak["n"] = json!(URL_SAFE_NO_PAD.encode([0xff; 128]));
    for document in [
        json!({"keys":[key_entry(0),key_entry(0)]}),
        json!({"keys":[wrong_issuer]}),
        json!({"keys":[private]}),
        json!({"keys":[weak]}),
        json!({"keys":[]}),
    ] {
        *source.document.lock().unwrap() = Ok(serde_json::to_vec(&document).unwrap());
        let (verifier, _) = verifier_with(config(), source.clone());
        assert_eq!(
            verifier.verify(&signed(0, &claims())).await.unwrap_err(),
            IdentityError::KeysUnavailable
        );
    }
    *source.document.lock().unwrap() = Ok(vec![b' '; 512 * 1024 + 1]);
    let (verifier, _) = verifier_with(config(), source.clone());
    assert!(verifier.verify(&signed(0, &claims())).await.is_err());
}

#[test]
fn configuration_cannot_disable_identity_or_request_arbitrary_authorities() {
    let serialized = serde_json::to_value(config()).unwrap();
    assert_eq!(
        serde_json::from_value::<EntraConfig>(serialized.clone()).unwrap(),
        config()
    );
    for (field, value) in [
        ("tenantId", json!("common")),
        ("tenantId", json!("00000000-0000-0000-0000-000000000000")),
        ("audience", json!("https://graph.microsoft.com")),
        ("allowedClients", json!([])),
        ("allowedClients", json!(["*"])),
        ("clockSkewSeconds", json!(86400)),
        ("issuer", json!("https://attacker.test")),
        ("actor", json!({"type":"delegatedUser","requiredScopes":[]})),
        ("actor", json!({"type":"workload","requiredRoles":[]})),
    ] {
        let mut invalid = serialized.clone();
        invalid[field] = value;
        assert!(
            serde_json::from_value::<EntraConfig>(invalid).is_err(),
            "{field}"
        );
    }
}

fn generic_policy() -> OAuth2ClaimsPolicy {
    serde_json::from_value(json!({
        "tenantId": "enterprise-test",
        "audience": "https://zuno.example.test/api",
        "allowedClients": ["web-client"],
        "requiredScopes": ["session:access"],
        "principalKind": "user",
        "actorClaim": {"claim":"principal_type","value":"user"}
    }))
    .unwrap()
}

fn generic_claims(issuer: &str) -> Value {
    let now = jsonwebtoken::get_current_timestamp();
    json!({
        "iss": issuer, "aud": "https://zuno.example.test/api",
        "sub": "opaque:subject/with+a-provider-specific-form",
        "client_id": "web-client", "scope": "session:access profile:read",
        "principal_type": "user", "iat": now - 10, "exp": now + 3600,
        "jti": "unique-token-identifier"
    })
}

fn generic_verifier(issuer: &str, profile: OAuth2JwtProfile) -> OAuth2JwtVerifier {
    let authority = OAuth2Authority::oidc(issuer).unwrap();
    let config = OAuth2JwtConfig::new(authority.clone(), generic_policy(), profile).unwrap();
    let source = Source::new();
    let mut entry = key_entry(0);
    entry.as_object_mut().unwrap().remove("issuer");
    entry.as_object_mut().unwrap().remove("use");
    *source.document.lock().unwrap() = Ok(serde_json::to_vec(&json!({"keys":[entry]})).unwrap());
    let cache =
        Arc::new(SigningKeyCache::new(authority, source, KeyCacheOptions::default()).unwrap());
    OAuth2JwtVerifier::with_keys(config, cache).unwrap()
}

fn rfc9068_token(claims: &Value) -> String {
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some("test-key-0".to_owned());
    header.typ = Some("at+jwt".to_owned());
    sign_with_header(0, &header, claims)
}

#[tokio::test]
async fn generic_oauth2_keeps_issuer_subjects_distinct_without_entra_claims() {
    let issuer_a = "https://identity.example.test/realm-a";
    let issuer_b = "https://identity.example.test/realm-b";
    let a = generic_verifier(issuer_a, OAuth2JwtProfile::Rfc9068);
    let b = generic_verifier(issuer_b, OAuth2JwtProfile::Rfc9068);
    let identity_a = a
        .verify(&rfc9068_token(&generic_claims(issuer_a)))
        .await
        .unwrap();
    let identity_b = b
        .verify(&rfc9068_token(&generic_claims(issuer_b)))
        .await
        .unwrap();
    assert_eq!(identity_a.tenant_id(), identity_b.tenant_id());
    assert_ne!(identity_a.principal_id(), identity_b.principal_id());
    assert_ne!(identity_a.client_id(), identity_b.client_id());
    assert_eq!(identity_a.kind(), VerifiedIdentityKind::DelegatedUser);
    assert!(
        a.verify(&rfc9068_token(&generic_claims(issuer_b)))
            .await
            .is_err()
    );
    let mut aliases = generic_claims(issuer_a);
    aliases["email"] = json!("a-new-display-alias@example.test");
    aliases["tid"] = json!(OTHER);
    aliases["oid"] = json!(OTHER);
    assert_eq!(
        a.verify(&rfc9068_token(&aliases))
            .await
            .unwrap()
            .principal_id(),
        identity_a.principal_id()
    );
}

#[tokio::test]
async fn generic_jwt_requires_access_token_profile_and_actor_classification() {
    let issuer = "https://identity.example.test";
    let verifier = generic_verifier(issuer, OAuth2JwtProfile::Rfc9068);
    // The same claims signed as an ID-token-style JWT cannot pass the API.
    assert_eq!(
        verifier
            .verify(&signed(0, &generic_claims(issuer)))
            .await
            .unwrap_err(),
        TokenRejection::UnsupportedToken.into()
    );
    for field in ["jti", "iat", "client_id", "sub", "scope", "principal_type"] {
        let mut missing = generic_claims(issuer);
        missing.as_object_mut().unwrap().remove(field);
        assert!(
            verifier.verify(&rfc9068_token(&missing)).await.is_err(),
            "{field}"
        );
    }
    for (field, value) in [
        ("client_id", json!("other-client")),
        ("scope", json!("session:access-extra")),
        ("principal_type", json!("workload")),
        ("cnf", json!({"jkt":"sender-constrained-key"})),
    ] {
        let mut invalid = generic_claims(issuer);
        invalid[field] = value;
        assert!(
            verifier.verify(&rfc9068_token(&invalid)).await.is_err(),
            "{field}"
        );
    }
    let provider = generic_verifier(
        issuer,
        OAuth2JwtProfile::Provider {
            access_token_claim: ClaimRequirement {
                claim: "token_use".to_owned(),
                value: "access".to_owned(),
            },
        },
    );
    let mut access = generic_claims(issuer);
    access["token_use"] = json!("id");
    assert!(provider.verify(&signed(0, &access)).await.is_err());
    access["token_use"] = json!("access");
    provider.verify(&signed(0, &access)).await.unwrap();
}

struct Introspector {
    response: Mutex<Result<Value, IdentityError>>,
    calls: AtomicUsize,
}

#[async_trait]
impl TokenIntrospector for Introspector {
    async fn introspect(&self, _: &str) -> Result<Value, IdentityError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.response.lock().unwrap().clone()
    }
}

#[tokio::test]
async fn introspection_checks_revocation_each_time_and_never_falls_back_to_jwt() {
    let issuer = "https://identity.example.test";
    let mut response = generic_claims(issuer);
    response["active"] = json!(true);
    response["token_type"] = json!("Bearer");
    let source = Arc::new(Introspector {
        response: Mutex::new(Ok(response.clone())),
        calls: AtomicUsize::new(0),
    });
    let config =
        OAuth2IntrospectionConfig::new(issuer, format!("{issuer}/introspection"), generic_policy())
            .unwrap();
    let verifier = OAuth2IntrospectionVerifier::with_introspector(config, source.clone());
    let identity = verifier.verify("opaque-access-token").await.unwrap();
    let jwt_identity = generic_verifier(issuer, OAuth2JwtProfile::Rfc9068)
        .verify(&rfc9068_token(&generic_claims(issuer)))
        .await
        .unwrap();
    assert_eq!(identity.principal_id(), jwt_identity.principal_id());
    response["active"] = json!(false);
    *source.response.lock().unwrap() = Ok(response.clone());
    assert!(verifier.verify("opaque-access-token").await.is_err());
    *source.response.lock().unwrap() = Err(IdentityError::IntrospectionUnavailable);
    assert_eq!(
        verifier.verify("opaque-access-token").await.unwrap_err(),
        IdentityError::IntrospectionUnavailable
    );
    assert_eq!(source.calls.load(Ordering::SeqCst), 3);
    response["active"] = json!(true);
    for (field, value) in [
        ("aud", json!("another-service")),
        ("iss", json!("https://other-issuer.test")),
        ("scope", json!("read")),
        ("client_id", json!("other-client")),
        ("exp", json!(1)),
        ("token_type", json!("DPoP")),
        ("active", json!("true")),
    ] {
        let mut invalid = response.clone();
        invalid[field] = value;
        *source.response.lock().unwrap() = Ok(invalid);
        assert!(
            verifier.verify("opaque-access-token").await.is_err(),
            "{field}"
        );
    }
    response.as_object_mut().unwrap().remove("exp");
    *source.response.lock().unwrap() = Ok(response);
    assert!(verifier.verify("opaque-access-token").await.is_err());
}

#[test]
fn generic_authority_trust_is_explicit_and_cannot_be_overridden_by_discovery() {
    let authority = OAuth2Authority::oidc("https://identity.example.test/realm-a").unwrap();
    assert_eq!(
        authority.discovery_url(),
        "https://identity.example.test/realm-a/.well-known/openid-configuration"
    );
    assert!(
        authority
            .check_jwks_url("https://identity.example.test/keys")
            .is_ok()
    );
    assert!(
        authority
            .check_jwks_url("https://another.example.test/keys")
            .is_err()
    );
    assert!(
        authority
            .check_jwks_url("https://identity.example.test:8443/keys")
            .is_err()
    );
    assert!(
        authority
            .check_jwks_url("https://user@identity.example.test/keys")
            .is_err()
    );
    let configured: OAuth2Authority = serde_json::from_value(json!({
        "issuer": "https://identity.example.test/realm-a",
        "additionalJwksOrigins": ["https://keys.example.test"]
    }))
    .unwrap();
    assert!(
        configured
            .check_jwks_url("https://keys.example.test/public/keys.json")
            .is_ok()
    );
    assert!(
        configured
            .check_jwks_url("http://keys.example.test/public/keys.json")
            .is_err()
    );
    assert!(
        serde_json::from_value::<OAuth2Authority>(json!({"issuer":"http://identity.example.test"}))
            .is_err()
    );
    let mut policy = serde_json::to_value(generic_policy()).unwrap();
    policy["actorClaim"] = json!({"claim":"email","value":"person@example.test"});
    assert!(serde_json::from_value::<OAuth2ClaimsPolicy>(policy).is_err());
}
