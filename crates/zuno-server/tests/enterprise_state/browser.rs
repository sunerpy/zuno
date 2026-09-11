use super::*;
use aws_lc_rs::{
    digest,
    rsa::{KeyPair, KeySize},
    signature::KeyPair as _,
};
use axum::{
    Form, Json,
    extract::{Request, State},
    http::{HeaderMap, header},
    response::{IntoResponse, Response},
    routing::get,
};
use base64::{
    Engine,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use std::collections::BTreeMap;
use zuno_identity::{
    KeyCacheOptions, OAuth2Authority, OAuth2JwtConfig, OAuth2JwtProfile, OAuth2JwtVerifier,
    OidcKeySource, SigningKeyCache,
    browser_login::BrowserLoginService,
    login::{CodeClientAuthMethod, HttpCodeExchange, OidcLoginClient, OidcLoginConfig},
    login_state::LoginStateCipher,
};
use zuno_server::enterprise_browser::*;

const CLIENT: &str = "enterprise-web";
const SECRET: &str = "fixture:secret+space value";

struct Code {
    subject: String,
    nonce: String,
    challenge: String,
    redirect: String,
}
struct Issuer {
    origin: String,
    key: KeyPair,
    codes: Mutex<BTreeMap<String, Code>>,
    calls: AtomicUsize,
    fail: std::sync::atomic::AtomicBool,
}
impl Issuer {
    fn sign(&self, typ: Option<&str>, claims: Value) -> String {
        let mut header = json!({"alg":"RS256","kid":"fixture"});
        if let Some(typ) = typ {
            header["typ"] = json!(typ)
        }
        let input = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header).unwrap()),
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
        );
        let mut signature = vec![0; self.key.public_modulus_len()];
        self.key
            .sign(
                &aws_lc_rs::signature::RSA_PKCS1_SHA256,
                &aws_lc_rs::rand::SystemRandom::new(),
                input.as_bytes(),
                &mut signature,
            )
            .unwrap();
        format!("{input}.{}", URL_SAFE_NO_PAD.encode(signature))
    }
}

async fn metadata(State(issuer): State<Arc<Issuer>>) -> Json<Value> {
    Json(json!({
        "issuer":issuer.origin,
        "authorization_endpoint":format!("{}/authorize",issuer.origin),
        "token_endpoint":format!("{}/token",issuer.origin),
        "jwks_uri":format!("{}/jwks",issuer.origin),
        "response_types_supported":["code"],
    }))
}
async fn keys(State(issuer): State<Arc<Issuer>>) -> Json<Value> {
    let public =
        aws_lc_rs::signature::RsaPublicKeyComponents::<Vec<u8>>::from(issuer.key.public_key());
    Json(
        json!({"keys":[{"kid":"fixture","kty":"RSA","use":"sig","alg":"RS256",
        "n":URL_SAFE_NO_PAD.encode(public.n),"e":URL_SAFE_NO_PAD.encode(public.e)}]}),
    )
}
async fn tokens(
    State(issuer): State<Arc<Issuer>>,
    headers: HeaderMap,
    Form(form): Form<BTreeMap<String, String>>,
) -> Response {
    issuer.calls.fetch_add(1, Ordering::SeqCst);
    let credentials = format!("{CLIENT}:fixture%3Asecret%2Bspace+value");
    assert_eq!(
        headers[header::AUTHORIZATION],
        format!("Basic {}", STANDARD.encode(credentials))
    );
    assert_eq!(form["grant_type"], "authorization_code");
    assert!(!form.contains_key("client_secret"));
    let Some(code) = issuer.codes.lock().unwrap().remove(&form["code"]) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    assert_eq!(form["redirect_uri"], code.redirect);
    assert_eq!(
        URL_SAFE_NO_PAD.encode(digest::digest(
            &digest::SHA256,
            form["code_verifier"].as_bytes()
        )),
        code.challenge
    );
    if issuer.fail.load(Ordering::SeqCst) {
        return (
            StatusCode::TEMPORARY_REDIRECT,
            [(header::LOCATION, "/must-not-retry")],
        )
            .into_response();
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let access = issuer.sign(
        Some("at+jwt"),
        json!({
            "iss":issuer.origin,"aud":"zuno-api","sub":code.subject,"client_id":CLIENT,
            "scope":"session:access","actor":"user","jti":uuid::Uuid::new_v4().to_string(),
            "iat":now,"exp":now+300,
        }),
    );
    let hash = digest::digest(&digest::SHA256, access.as_bytes());
    let id = issuer.sign(
        None,
        json!({
            "iss":issuer.origin,"aud":CLIENT,"sub":code.subject,"nonce":code.nonce,
            "iat":now,"exp":now+300,"at_hash":URL_SAFE_NO_PAD.encode(&hash.as_ref()[..16]),
        }),
    );
    Json(json!({"access_token":access,"id_token":id,"token_type":"Bearer"})).into_response()
}

async fn browser_service(
    fixture: &Fixture,
    backend: &PostgresBackend,
    issuer: &str,
    redirect: &url::Url,
) -> EnterpriseBrowser {
    let authority = OAuth2Authority::oidc(issuer).unwrap();
    let root = std::fs::read(&fixture.root_certificate).unwrap();
    let source =
        Arc::new(OidcKeySource::with_root_certificate(authority.clone(), Some(&root)).unwrap());
    let metadata = source.discovery_document().await.unwrap();
    let keys = Arc::new(
        SigningKeyCache::new(authority.clone(), source, KeyCacheOptions::default()).unwrap(),
    );
    let policy = serde_json::from_value(json!({
        "tenantId":"browser-http","audience":"zuno-api","allowedClients":[CLIENT],
        "requiredScopes":["session:access"],"principalKind":"user","actorClaim":{"claim":"actor","value":"user"},
    })).unwrap();
    let access = Arc::new(
        OAuth2JwtVerifier::with_keys(
            OAuth2JwtConfig::new(authority.clone(), policy, OAuth2JwtProfile::Rfc9068).unwrap(),
            keys.clone(),
        )
        .unwrap(),
    );
    let config = OidcLoginConfig::from_metadata(
        authority,
        CLIENT.to_owned(),
        redirect.as_str(),
        ["openid".to_owned(), "session:access".to_owned()].into(),
        &metadata,
    )
    .unwrap();
    let client = Arc::new(
        OidcLoginClient::new(
            config,
            keys,
            Arc::new(
                HttpCodeExchange::with_root_certificate(
                    CodeClientAuthMethod::Basic,
                    zuno_auth::Secret::new(SECRET),
                    Some(&root),
                )
                .unwrap(),
            ),
            access,
        )
        .unwrap(),
    );
    let store = Arc::new(
        backend
            .browser_sessions(TenantId::new("browser-http").unwrap(), Default::default())
            .unwrap(),
    );
    let cipher = Arc::new(
        LoginStateCipher::new(
            "current".to_owned(),
            [("current".to_owned(), vec![8; 32])].into(),
        )
        .unwrap(),
    );
    EnterpriseBrowser::new(Arc::new(BrowserLoginService::new(
        client,
        cipher,
        store.clone(),
        store,
    )))
    .unwrap()
}

#[derive(Clone)]
struct Replicas {
    first: Router,
    second: Router,
}
async fn replicas(State(replicas): State<Replicas>, request: Request) -> Response {
    use tower::ServiceExt as _;
    let router = if request.uri().path() == LOGIN_PATH {
        replicas.first
    } else {
        replicas.second
    };
    router.oneshot(request).await.unwrap()
}

fn cookie(headers: &HeaderMap, name: &str) -> String {
    headers
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|header| header.to_str().ok())
        .find(|value| value.starts_with(&format!("{name}=")))
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned()
}

async fn start(
    client: &reqwest::Client,
    base: &url::Url,
    issuer: &Issuer,
    subject: &str,
) -> (url::Url, String) {
    let response = client
        .post(base.join(LOGIN_PATH).unwrap())
        .header(header::ORIGIN, base.origin().ascii_serialization())
        .header(CSRF_HEADER, "1")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    let binding = cookie(response.headers(), LOGIN_COOKIE);
    let attributes = response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .next()
        .unwrap()
        .to_str()
        .unwrap();
    for attribute in ["Secure", "HttpOnly", "SameSite=Lax", "Path=/"] {
        assert!(attributes.contains(attribute))
    }
    let location = url::Url::parse(response.headers()[header::LOCATION].to_str().unwrap()).unwrap();
    let fields = location
        .query_pairs()
        .into_owned()
        .collect::<BTreeMap<_, _>>();
    assert_eq!(fields["code_challenge_method"], "S256");
    assert!(!fields.contains_key("code_verifier"));
    let code = uuid::Uuid::new_v4().to_string();
    issuer.codes.lock().unwrap().insert(
        code.clone(),
        Code {
            subject: subject.to_owned(),
            nonce: fields["nonce"].clone(),
            challenge: fields["code_challenge"].clone(),
            redirect: fields["redirect_uri"].clone(),
        },
    );
    let mut callback = base.join(CALLBACK_PATH).unwrap();
    callback.query_pairs_mut().extend_pairs([
        ("state", fields["state"].as_str()),
        ("code", &code),
        ("iss", &issuer.origin),
    ]);
    (callback, binding)
}

#[tokio::test]
#[ignore = "run scripts/check_enterprise_postgres.py for the isolated TLS/PostgreSQL fixture"]
async fn browsers_use_pkce_across_bff_replicas_and_keep_sessions_private_revocable_and_csrf_bound()
{
    let fixture: Fixture = serde_json::from_slice(
        &std::fs::read(std::env::var("ZUNO_POSTGRES_TEST_CONFIG").unwrap()).unwrap(),
    )
    .unwrap();
    let root = fixture
        .options(&fixture.admin_url, "postgres")
        .connect()
        .await
        .unwrap();
    raw_sql("CREATE DATABASE zuno_browser_fixture OWNER zuno_preview_migrator")
        .execute(&root)
        .await
        .unwrap();
    let admin = fixture
        .options(&fixture.admin_url, "zuno_browser_fixture")
        .connect()
        .await
        .unwrap();
    let migrator = fixture
        .options(&fixture.migration_url, "zuno_browser_fixture")
        .connect()
        .await
        .unwrap();
    migrate(&migrator, &fixture.runtime_role).await.unwrap();
    let backend =
        PostgresBackend::connect(fixture.options(&fixture.runtime_url, "zuno_browser_fixture"))
            .await
            .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let issuer = Arc::new(Issuer {
        origin: format!(
            "https://localhost:{}",
            listener.local_addr().unwrap().port()
        ),
        key: KeyPair::generate(KeySize::Rsa2048).unwrap(),
        codes: Mutex::new(BTreeMap::new()),
        calls: AtomicUsize::new(0),
        fail: std::sync::atomic::AtomicBool::new(false),
    });
    let routes = Router::new()
        .route("/.well-known/openid-configuration", get(metadata))
        .route("/jwks", get(keys))
        .route("/token", post(tokens))
        .with_state(issuer.clone());
    let (_idp, idp_server) = tls_server_at(routes, &fixture, listener).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = url::Url::parse(&format!(
        "https://localhost:{}/",
        listener.local_addr().unwrap().port()
    ))
    .unwrap();
    let first = browser_service(
        &fixture,
        &backend,
        &issuer.origin,
        &base.join(CALLBACK_PATH).unwrap(),
    )
    .await;
    let second = browser_service(
        &fixture,
        &backend,
        &issuer.origin,
        &base.join(CALLBACK_PATH).unwrap(),
    )
    .await;
    let routes = Router::new().fallback(replicas).with_state(Replicas {
        first: first.router(),
        second: second.router(),
    });
    let (_, bff_server) = tls_server_at(routes, &fixture, listener).await;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .add_root_certificate(
            reqwest::Certificate::from_pem(&std::fs::read(&fixture.root_certificate).unwrap())
                .unwrap(),
        )
        .build()
        .unwrap();
    assert_eq!(
        client
            .post(base.join(LOGIN_PATH).unwrap())
            .header(header::HOST, "attacker.example")
            .header(header::ORIGIN, base.origin().ascii_serialization())
            .header(CSRF_HEADER, "1")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        client
            .post(base.join(LOGIN_PATH).unwrap())
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        client
            .post(base.join(LOGIN_PATH).unwrap())
            .header(header::ORIGIN, "https://attacker.example")
            .header(CSRF_HEADER, "1")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    let (callback, binding) = start(&client, &base, &issuer, "alice").await;
    let mut invalid_callback = callback.clone();
    invalid_callback.set_query(Some("state=wrong&code=wrong"));
    let invalid = client
        .get(invalid_callback)
        .header(header::COOKIE, &binding)
        .send()
        .await
        .unwrap();
    assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
    assert!(
        !invalid.headers().contains_key(header::SET_COOKIE),
        "an unbound callback must not clear an active browser's login cookie"
    );
    let other_binding = format!("{LOGIN_COOKIE}={}", URL_SAFE_NO_PAD.encode([9; 32]));
    assert_eq!(
        client
            .get(callback.clone())
            .header(header::COOKIE, other_binding)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(issuer.calls.load(Ordering::SeqCst), 0);
    let response = client
        .get(callback.clone())
        .header(header::COOKIE, &binding)
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::SEE_OTHER,
        "{}",
        response.text().await.unwrap()
    );
    let alice = cookie(response.headers(), SESSION_COOKIE);
    assert_eq!(response.headers()[header::LOCATION], "/");
    assert_eq!(issuer.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        client
            .get(callback)
            .header(header::COOKIE, binding)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(issuer.calls.load(Ordering::SeqCst), 1);
    let a: Value = client
        .get(base.join(SESSION_PATH).unwrap())
        .header(header::COOKIE, &alice)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(a["tenantId"], "browser-http");
    assert!(a.get("accessToken").is_none() && a.get("idToken").is_none());
    let (callback, binding) = start(&client, &base, &issuer, "bob").await;
    let response = client
        .get(callback)
        .header(header::COOKIE, binding)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let bob = cookie(response.headers(), SESSION_COOKIE);
    let b: Value = client
        .get(base.join(SESSION_PATH).unwrap())
        .header(header::COOKIE, &bob)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_ne!(a["principalId"], b["principalId"]);
    assert_eq!(
        client
            .get(base.join(SESSION_PATH).unwrap())
            .header(header::COOKIE, format!("{alice}; {bob}"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    raw_sql(
        "REVOKE SELECT,DELETE ON zuno_enterprise_preview.browser_session FROM zuno_preview_runtime",
    )
    .execute(&admin)
    .await
    .unwrap();
    assert_eq!(
        client
            .get(base.join(SESSION_PATH).unwrap())
            .header(header::COOKIE, &alice)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    let failed_logout = client
        .post(base.join(LOGOUT_PATH).unwrap())
        .header(header::COOKIE, &alice)
        .header(header::ORIGIN, base.origin().ascii_serialization())
        .header(CSRF_HEADER, "1")
        .send()
        .await
        .unwrap();
    assert_eq!(failed_logout.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(!failed_logout.headers().contains_key(header::SET_COOKIE));
    raw_sql(
        "GRANT SELECT,DELETE ON zuno_enterprise_preview.browser_session TO zuno_preview_runtime",
    )
    .execute(&admin)
    .await
    .unwrap();
    assert_eq!(
        client
            .post(base.join(LOGOUT_PATH).unwrap())
            .header(header::COOKIE, &alice)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        client
            .post(base.join(LOGOUT_PATH).unwrap())
            .header(header::COOKIE, &alice)
            .header(header::ORIGIN, base.origin().ascii_serialization())
            .header(CSRF_HEADER, "1")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        client
            .get(base.join(SESSION_PATH).unwrap())
            .header(header::COOKIE, &alice)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        client
            .get(base.join(SESSION_PATH).unwrap())
            .header(header::COOKIE, &bob)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    issuer.fail.store(true, Ordering::SeqCst);
    let (callback, binding) = start(&client, &base, &issuer, "alice").await;
    assert_eq!(
        client
            .get(callback.clone())
            .header(header::COOKIE, &binding)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        issuer.calls.load(Ordering::SeqCst),
        3,
        "token POSTs neither redirect nor retry"
    );
    assert_eq!(
        client
            .get(callback)
            .header(header::COOKIE, &binding)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(issuer.calls.load(Ordering::SeqCst), 3);
    let leaked: i64 = sqlx_core::query_scalar::query_scalar(
        "SELECT count(*) FROM zuno_enterprise_preview.browser_session WHERE identity::text LIKE $1",
    )
    .bind(format!("%{}%", bob.split_once('=').unwrap().1))
    .fetch_one(&admin)
    .await
    .unwrap();
    assert_eq!(leaked, 0, "only credential hashes are persisted");
    bff_server.abort();
    idp_server.abort();
    let _ = bff_server.await;
    let _ = idp_server.await;
}
