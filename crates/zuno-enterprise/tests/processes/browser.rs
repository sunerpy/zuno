//! A real authorization-code issuer and Chromium client for the executable test.
//! This fixture is never linked into an enterprise service.
use super::*;
use aws_lc_rs::digest;
use axum::{Form, extract::Query, response::Redirect};
use base64::engine::general_purpose::STANDARD;

pub struct Code {
    nonce: String,
    challenge: String,
    redirect: String,
}

pub async fn authorize(
    State(issuer): State<Arc<Issuer>>,
    Query(params): Query<BTreeMap<String, String>>,
) -> Redirect {
    assert_eq!(params["client_id"], "web");
    assert_eq!(params["response_type"], "code");
    assert_eq!(params["code_challenge_method"], "S256");
    let code = uuid::Uuid::new_v4().to_string();
    let mut redirect = url::Url::parse(&params["redirect_uri"]).unwrap();
    issuer.codes.lock().unwrap().insert(
        code.clone(),
        Code {
            nonce: params["nonce"].clone(),
            challenge: params["code_challenge"].clone(),
            redirect: redirect.to_string(),
        },
    );
    redirect
        .query_pairs_mut()
        .append_pair("code", &code)
        .append_pair("state", &params["state"])
        .append_pair("iss", &issuer.origin);
    Redirect::to(redirect.as_str())
}

pub async fn exchange(
    State(issuer): State<Arc<Issuer>>,
    headers: HeaderMap,
    Form(form): Form<BTreeMap<String, String>>,
) -> Json<Value> {
    assert_eq!(
        headers[header::AUTHORIZATION],
        format!("Basic {}", STANDARD.encode("web:browser-fixture-secret"))
    );
    assert_eq!(form["grant_type"], "authorization_code");
    let code = issuer.codes.lock().unwrap().remove(&form["code"]).unwrap();
    assert_eq!(form["redirect_uri"], code.redirect);
    assert_eq!(
        URL_SAFE_NO_PAD.encode(digest::digest(
            &digest::SHA256,
            form["code_verifier"].as_bytes()
        )),
        code.challenge,
    );
    let access = issuer.token("alice", "web", true);
    let hash = digest::digest(&digest::SHA256, access.as_bytes());
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let id = issuer.sign("JWT", json!({
        "iss":issuer.origin,"aud":"web","sub":"alice","nonce":code.nonce,"iat":now,"exp":now+3600,
        "at_hash":URL_SAFE_NO_PAD.encode(&hash.as_ref()[..16]),
    }));
    Json(json!({"access_token":access,"id_token":id,"token_type":"Bearer"}))
}

pub fn config(root: &Path, fixture: &Fixture, issuer: &Issuer, control: &str) -> BrowserConfig {
    let secret = root.join("browser.secret");
    let key = root.join("browser.key");
    write(&secret, "browser-fixture-secret");
    write(&key, [9; 32]);
    BrowserConfig {
        authority: zuno_identity::OAuth2Authority::oidc(&issuer.origin).unwrap(),
        client_id: "web".to_owned(),
        client_secret_file: secret,
        redirect_uri: format!("{control}auth/callback"),
        scopes: ["openid".to_owned(), "agent".to_owned()].into(),
        root_certificate: Some(fixture.root_certificate.clone()),
        encryption_keys: KeyFiles {
            active: "browser".to_owned(),
            keys: vec![KeyFile {
                id: "browser".to_owned(),
                path: key,
            }],
        },
    }
}

pub async fn verify(root: &Path, control: &str) {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut command = tokio::process::Command::new("node");
    command
        .arg(repository.join("enterprise/web/tests/native.mjs"))
        .arg(control)
        .current_dir(&repository)
        .env("ZUNO_WEB_NATIVE_OUTPUT", root.join("browser.png"))
        .env("NO_PROXY", "localhost,127.0.0.1")
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(90), command.output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        output.status.success(),
        "native Web failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let evidence = repository.join("target/enterprise-validation");
    std::fs::create_dir_all(&evidence).unwrap();
    std::fs::copy(root.join("browser.png"), evidence.join("web-native.png")).unwrap();
    std::fs::write(evidence.join("web-native.txt"), &output.stdout).unwrap();
    println!("{}", String::from_utf8_lossy(&output.stdout));
}
