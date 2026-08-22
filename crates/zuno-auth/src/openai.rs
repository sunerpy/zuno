//! OpenAI ChatGPT OAuth authorization and refresh.
//!
//! This is a native implementation of the public Codex login protocol: browser
//! authorization with PKCE, device-code authorization for headless hosts, token
//! exchange, and refresh. API-key authentication remains a separate login method.

use std::fmt;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use reqwest::StatusCode;
use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use url::Url;

use crate::{Credential, Secret};

/// Official OpenAI OAuth issuer used by Codex.
pub const OPENAI_OAUTH_ISSUER: &str = "https://auth.openai.com";
/// Public OAuth client id used by the Codex CLI.
pub const OPENAI_OAUTH_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
/// ChatGPT-backed Responses endpoint used for subscription authentication.
pub const CHATGPT_CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";

const DEVICE_AUTH_PATH: &str = "/api/accounts/deviceauth";
const TOKEN_PATH: &str = "/oauth/token";
const DEVICE_VERIFY_PATH: &str = "/codex/device";
const DEVICE_REDIRECT_PATH: &str = "/deviceauth/callback";
const DEFAULT_DEVICE_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const DEFAULT_TOKEN_LIFETIME_SECS: u64 = 3_600;
const REFRESH_SKEW_MS: u64 = 30_000;
const OAUTH_SCOPE: &str =
    "openid profile email offline_access api.connectors.read api.connectors.invoke";

/// Which remote OAuth operation failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenAiOauthOperation {
    /// Request the browser authorization page.
    BrowserAuthorization,
    /// Exchange an authorization code for tokens.
    TokenExchange,
    /// Request a device code.
    DeviceCode,
    /// Poll for completion of device authorization.
    DevicePoll,
    /// Refresh an expired access token.
    Refresh,
}

impl fmt::Display for OpenAiOauthOperation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::BrowserAuthorization => "OpenAI browser authorization",
            Self::TokenExchange => "OpenAI token exchange",
            Self::DeviceCode => "OpenAI device-code request",
            Self::DevicePoll => "OpenAI device-code polling",
            Self::Refresh => "OpenAI token refresh",
        })
    }
}

/// A typed OpenAI OAuth failure.
#[derive(Debug, thiserror::Error)]
pub enum OpenAiOauthError {
    /// The configured issuer is not a valid URL.
    #[error("OpenAI OAuth issuer is not a valid URL")]
    InvalidIssuer(#[source] url::ParseError),
    /// An HTTP request could not be completed.
    #[error("{operation} request failed")]
    Transport {
        /// Operation in progress.
        operation: OpenAiOauthOperation,
        /// HTTP failure.
        #[source]
        source: reqwest::Error,
    },
    /// The peer returned a non-success status.
    #[error("{operation} returned HTTP {status}")]
    Rejected {
        /// Operation in progress.
        operation: OpenAiOauthOperation,
        /// HTTP status.
        status: u16,
    },
    /// A successful response omitted a required field.
    #[error("{operation} response omitted required field {field}")]
    MissingField {
        /// Operation in progress.
        operation: OpenAiOauthOperation,
        /// Missing JSON field.
        field: &'static str,
    },
    /// A token that should be a JWT could not be decoded.
    #[error("OpenAI OAuth token is not a valid JWT")]
    InvalidToken(#[source] TokenDecodeError),
    /// Device authorization was not completed before its deadline.
    #[error("OpenAI device authorization timed out")]
    DeviceTimeout,
}

impl OpenAiOauthError {
    /// Whether retrying after backoff can reasonably succeed.
    #[must_use]
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Transport { .. } => true,
            Self::Rejected { status, .. } => *status == 429 || (500..=599).contains(status),
            Self::InvalidIssuer(_)
            | Self::MissingField { .. }
            | Self::InvalidToken(_)
            | Self::DeviceTimeout => false,
        }
    }
}

/// Browser authorization state that must survive until the loopback callback.
#[derive(Clone)]
pub struct BrowserAuthorization {
    url: Url,
    redirect_uri: String,
    state: Secret,
    verifier: Secret,
}

impl fmt::Debug for BrowserAuthorization {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BrowserAuthorization")
            .field("redirect_uri", &self.redirect_uri)
            .field("state", &self.state)
            .field("verifier", &self.verifier)
            .finish_non_exhaustive()
    }
}

impl BrowserAuthorization {
    /// URL the user opens.
    #[must_use]
    pub fn url(&self) -> &Url {
        &self.url
    }

    /// Whether a callback carries the state created for this authorization.
    #[must_use]
    pub fn state_matches(&self, state: &str) -> bool {
        self.state.expose() == state
    }
}

/// Pending OpenAI device authorization.
#[derive(Clone)]
pub struct DeviceAuthorization {
    verification_url: Url,
    user_code: Secret,
    device_auth_id: Secret,
    interval: Duration,
}

impl fmt::Debug for DeviceAuthorization {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DeviceAuthorization")
            .field("verification_url", &self.verification_url)
            .field("user_code", &self.user_code)
            .field("device_auth_id", &self.device_auth_id)
            .field("interval", &self.interval)
            .finish()
    }
}

impl DeviceAuthorization {
    /// URL the user opens on any device.
    #[must_use]
    pub fn verification_url(&self) -> &Url {
        &self.verification_url
    }

    /// One-time code entered at [`Self::verification_url`].
    #[must_use]
    pub fn user_code(&self) -> &str {
        self.user_code.expose()
    }
}

/// Native client for OpenAI ChatGPT OAuth.
#[derive(Clone, Debug)]
pub struct OpenAiOauthClient {
    http: reqwest::Client,
    issuer: Url,
    client_id: String,
    device_timeout: Duration,
}

impl OpenAiOauthClient {
    /// Production OpenAI OAuth client.
    #[must_use]
    pub fn production() -> Self {
        Self {
            http: reqwest::Client::new(),
            issuer: Url::parse(OPENAI_OAUTH_ISSUER)
                .unwrap_or_else(|error| panic!("fixed OpenAI issuer is invalid: {error}")),
            client_id: OPENAI_OAUTH_CLIENT_ID.to_owned(),
            device_timeout: DEFAULT_DEVICE_TIMEOUT,
        }
    }

    /// Client pointed at an explicit issuer, primarily for private deployments and
    /// deterministic tests.
    pub fn with_issuer(
        issuer: &str,
        client_id: impl Into<String>,
    ) -> Result<Self, OpenAiOauthError> {
        Ok(Self {
            http: reqwest::Client::new(),
            issuer: Url::parse(issuer).map_err(OpenAiOauthError::InvalidIssuer)?,
            client_id: client_id.into(),
            device_timeout: DEFAULT_DEVICE_TIMEOUT,
        })
    }

    /// Override the device-flow deadline.
    #[must_use]
    pub const fn with_device_timeout(mut self, timeout: Duration) -> Self {
        self.device_timeout = timeout;
        self
    }

    /// Prepare browser OAuth with PKCE and a CSRF state value.
    pub fn browser_authorization(
        &self,
        redirect_uri: impl Into<String>,
    ) -> Result<BrowserAuthorization, OpenAiOauthError> {
        let redirect_uri = redirect_uri.into();
        let verifier = random_urlsafe();
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let state = random_urlsafe();
        let mut url = self.endpoint("/oauth/authorize")?;
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &self.client_id)
            .append_pair("redirect_uri", &redirect_uri)
            .append_pair("scope", OAUTH_SCOPE)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("id_token_add_organizations", "true")
            .append_pair("codex_cli_simplified_flow", "true")
            .append_pair("state", &state)
            .append_pair("originator", "zuno");
        Ok(BrowserAuthorization {
            url,
            redirect_uri,
            state: Secret::new(state),
            verifier: Secret::new(verifier),
        })
    }

    /// Exchange the browser callback's authorization code.
    pub async fn complete_browser_authorization(
        &self,
        authorization: &BrowserAuthorization,
        code: &str,
    ) -> Result<Credential, OpenAiOauthError> {
        self.exchange_code(code, &authorization.redirect_uri, &authorization.verifier)
            .await
    }

    /// Request a headless device code.
    pub async fn request_device_authorization(
        &self,
    ) -> Result<DeviceAuthorization, OpenAiOauthError> {
        #[derive(serde::Serialize)]
        struct Request<'a> {
            client_id: &'a str,
        }

        let operation = OpenAiOauthOperation::DeviceCode;
        let response = self
            .http
            .post(self.endpoint(&format!("{DEVICE_AUTH_PATH}/usercode"))?)
            .header("user-agent", user_agent())
            .json(&Request {
                client_id: &self.client_id,
            })
            .send()
            .await
            .map_err(|source| OpenAiOauthError::Transport { operation, source })?;
        let response = success(response, operation)?;
        let body: DeviceCodeResponse = response
            .json()
            .await
            .map_err(|source| OpenAiOauthError::Transport { operation, source })?;
        let device_auth_id = required(body.device_auth_id, operation, "device_auth_id")?;
        let user_code = required(body.user_code, operation, "user_code")?;
        Ok(DeviceAuthorization {
            verification_url: self.endpoint(DEVICE_VERIFY_PATH)?,
            user_code: Secret::new(user_code),
            device_auth_id: Secret::new(device_auth_id),
            interval: Duration::from_secs(body.interval.seconds().max(1)),
        })
    }

    /// Poll and complete a previously requested device authorization.
    pub async fn complete_device_authorization(
        &self,
        authorization: &DeviceAuthorization,
    ) -> Result<Credential, OpenAiOauthError> {
        #[derive(serde::Serialize)]
        struct Request<'a> {
            device_auth_id: &'a str,
            user_code: &'a str,
        }

        let operation = OpenAiOauthOperation::DevicePoll;
        let deadline = Instant::now() + self.device_timeout;
        loop {
            let response = self
                .http
                .post(self.endpoint(&format!("{DEVICE_AUTH_PATH}/token"))?)
                .header("user-agent", user_agent())
                .json(&Request {
                    device_auth_id: authorization.device_auth_id.expose(),
                    user_code: authorization.user_code.expose(),
                })
                .send()
                .await
                .map_err(|source| OpenAiOauthError::Transport { operation, source })?;
            if response.status().is_success() {
                let body: DevicePollResponse = response
                    .json()
                    .await
                    .map_err(|source| OpenAiOauthError::Transport { operation, source })?;
                let code = required(body.authorization_code, operation, "authorization_code")?;
                let verifier = required(body.code_verifier, operation, "code_verifier")?;
                let redirect = self.endpoint(DEVICE_REDIRECT_PATH)?.to_string();
                return self
                    .exchange_code(&code, &redirect, &Secret::new(verifier))
                    .await;
            }
            if !matches!(
                response.status(),
                StatusCode::FORBIDDEN | StatusCode::NOT_FOUND
            ) {
                return Err(OpenAiOauthError::Rejected {
                    operation,
                    status: response.status().as_u16(),
                });
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(OpenAiOauthError::DeviceTimeout);
            }
            tokio::time::sleep(authorization.interval.min(deadline - now)).await;
        }
    }

    /// Refresh an OAuth credential while preserving its account and enterprise
    /// metadata when the response does not replace them.
    pub async fn refresh(&self, credential: &Credential) -> Result<Credential, OpenAiOauthError> {
        let Credential::Oauth {
            refresh,
            account_id,
            enterprise_url,
            ..
        } = credential
        else {
            return Ok(credential.clone());
        };
        let operation = OpenAiOauthOperation::Refresh;
        let response = self
            .http
            .post(self.endpoint(TOKEN_PATH)?)
            .header("user-agent", user_agent())
            .form(&[
                ("client_id", self.client_id.as_str()),
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh.expose()),
            ])
            .send()
            .await
            .map_err(|source| OpenAiOauthError::Transport { operation, source })?;
        let response = success(response, operation)?;
        let body: TokenResponse = response
            .json()
            .await
            .map_err(|source| OpenAiOauthError::Transport { operation, source })?;
        let access = required(body.access_token, operation, "access_token")?;
        let next_refresh = body
            .refresh_token
            .unwrap_or_else(|| refresh.expose().to_owned());
        let discovered_account = body
            .id_token
            .as_deref()
            .and_then(account_id_from_jwt)
            .or_else(|| account_id_from_jwt(&access))
            .or_else(|| account_id.clone());
        Ok(Credential::Oauth {
            expires: expiration_millis(&access, body.expires_in),
            access: Secret::new(access),
            refresh: Secret::new(next_refresh),
            account_id: discovered_account,
            enterprise_url: enterprise_url.clone(),
        })
    }

    async fn exchange_code(
        &self,
        code: &str,
        redirect_uri: &str,
        verifier: &Secret,
    ) -> Result<Credential, OpenAiOauthError> {
        let operation = OpenAiOauthOperation::TokenExchange;
        let response = self
            .http
            .post(self.endpoint(TOKEN_PATH)?)
            .header("user-agent", user_agent())
            .form(&[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("redirect_uri", redirect_uri),
                ("client_id", self.client_id.as_str()),
                ("code_verifier", verifier.expose()),
            ])
            .send()
            .await
            .map_err(|source| OpenAiOauthError::Transport { operation, source })?;
        let response = success(response, operation)?;
        let body: TokenResponse = response
            .json()
            .await
            .map_err(|source| OpenAiOauthError::Transport { operation, source })?;
        let access = required(body.access_token, operation, "access_token")?;
        let refresh = required(body.refresh_token, operation, "refresh_token")?;
        let account_id = body
            .id_token
            .as_deref()
            .and_then(account_id_from_jwt)
            .or_else(|| account_id_from_jwt(&access));
        Ok(Credential::Oauth {
            expires: expiration_millis(&access, body.expires_in),
            access: Secret::new(access),
            refresh: Secret::new(refresh),
            account_id,
            enterprise_url: None,
        })
    }

    fn endpoint(&self, path: &str) -> Result<Url, OpenAiOauthError> {
        let mut base = self.issuer.as_str().trim_end_matches('/').to_owned();
        base.push_str(path);
        Url::parse(&base).map_err(OpenAiOauthError::InvalidIssuer)
    }
}

impl Default for OpenAiOauthClient {
    fn default() -> Self {
        Self::production()
    }
}

/// Whether an OAuth credential should be refreshed before use.
#[must_use]
pub fn needs_refresh(credential: &Credential) -> bool {
    matches!(
        credential,
        Credential::Oauth { expires, .. }
            if *expires <= now_millis().saturating_add(REFRESH_SKEW_MS)
    )
}

fn success(
    response: reqwest::Response,
    operation: OpenAiOauthOperation,
) -> Result<reqwest::Response, OpenAiOauthError> {
    if response.status().is_success() {
        Ok(response)
    } else {
        Err(OpenAiOauthError::Rejected {
            operation,
            status: response.status().as_u16(),
        })
    }
}

fn required(
    value: Option<String>,
    operation: OpenAiOauthOperation,
    field: &'static str,
) -> Result<String, OpenAiOauthError> {
    value
        .filter(|value| !value.trim().is_empty())
        .ok_or(OpenAiOauthError::MissingField { operation, field })
}

fn random_urlsafe() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

fn user_agent() -> String {
    format!("zuno/{}", env!("CARGO_PKG_VERSION"))
}

fn expiration_millis(access_token: &str, expires_in: Option<u64>) -> u64 {
    expires_in
        .map(|seconds| now_millis().saturating_add(seconds.saturating_mul(1_000)))
        .or_else(|| jwt_expiration_millis(access_token).ok().flatten())
        .unwrap_or_else(|| {
            now_millis().saturating_add(DEFAULT_TOKEN_LIFETIME_SECS.saturating_mul(1_000))
        })
}

fn now_millis() -> u64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    u64::try_from(millis).unwrap_or(u64::MAX)
}

fn account_id_from_jwt(jwt: &str) -> Option<String> {
    decode_jwt::<Claims>(jwt).ok().and_then(|claims| {
        claims
            .chatgpt_account_id
            .or_else(|| claims.auth.and_then(|auth| auth.chatgpt_account_id))
            .or_else(|| {
                claims
                    .organizations
                    .into_iter()
                    .next()
                    .map(|organization| organization.id)
            })
    })
}

/// ChatGPT compute-residency value carried by an OAuth access token.
#[must_use]
pub fn residency_from_jwt(jwt: &str) -> Option<String> {
    decode_jwt::<Claims>(jwt).ok().and_then(|claims| {
        claims
            .auth
            .and_then(|auth| auth.chatgpt_compute_residency)
            .or(claims.chatgpt_compute_residency)
            .filter(|residency| residency != "no_constraint")
    })
}

fn jwt_expiration_millis(jwt: &str) -> Result<Option<u64>, TokenDecodeError> {
    let claims: StandardClaims = decode_jwt(jwt)?;
    Ok(claims.exp.map(|seconds| seconds.saturating_mul(1_000)))
}

fn decode_jwt<T: serde::de::DeserializeOwned>(jwt: &str) -> Result<T, TokenDecodeError> {
    let mut parts = jwt.split('.');
    let (Some(header), Some(payload), Some(signature), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(TokenDecodeError::Format);
    };
    if header.is_empty() || payload.is_empty() || signature.is_empty() {
        return Err(TokenDecodeError::Format);
    }
    let bytes = URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(TokenDecodeError::Base64)?;
    serde_json::from_slice(&bytes).map_err(TokenDecodeError::Json)
}

/// JWT payload decoding failure.
#[derive(Debug, thiserror::Error)]
pub enum TokenDecodeError {
    /// The token does not have three non-empty dot-separated parts.
    #[error("invalid JWT format")]
    Format,
    /// The payload is not URL-safe base64.
    #[error("invalid JWT payload encoding")]
    Base64(#[source] base64::DecodeError),
    /// The decoded payload is not the expected JSON object.
    #[error("invalid JWT payload JSON")]
    Json(#[source] serde_json::Error),
}

#[derive(Debug, Deserialize)]
struct DeviceCodeResponse {
    #[serde(default)]
    device_auth_id: Option<String>,
    #[serde(default, alias = "usercode")]
    user_code: Option<String>,
    #[serde(default)]
    interval: Interval,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(untagged)]
enum Interval {
    #[default]
    Missing,
    Number(u64),
    Text(String),
}

impl Interval {
    fn seconds(self) -> u64 {
        match self {
            Self::Missing => 5,
            Self::Number(value) => value,
            Self::Text(value) => value.trim().parse().unwrap_or(5),
        }
    }
}

#[derive(Debug, Deserialize)]
struct DevicePollResponse {
    #[serde(default)]
    authorization_code: Option<String>,
    #[serde(default)]
    code_verifier: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct Claims {
    #[serde(default)]
    chatgpt_account_id: Option<String>,
    #[serde(default)]
    chatgpt_compute_residency: Option<String>,
    #[serde(rename = "https://api.openai.com/auth", default)]
    auth: Option<AuthClaims>,
    #[serde(default)]
    organizations: Vec<OrganizationClaims>,
}

#[derive(Debug, Deserialize)]
struct AuthClaims {
    #[serde(default)]
    chatgpt_account_id: Option<String>,
    #[serde(default)]
    chatgpt_compute_residency: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OrganizationClaims {
    id: String,
}

#[derive(Debug, Deserialize)]
struct StandardClaims {
    #[serde(default)]
    exp: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::extract::State;
    use axum::routing::post;
    use std::sync::{Arc, Mutex};

    fn jwt(payload: serde_json::Value) -> String {
        format!(
            "e30.{}.signature",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).expect("JSON"))
        )
    }

    #[test]
    fn browser_authorization_uses_pkce_state_and_offline_access() {
        let client =
            OpenAiOauthClient::with_issuer("https://auth.example.test", "client").expect("client");
        let authorization = client
            .browser_authorization("http://localhost:1455/auth/callback")
            .expect("authorization");
        let query = authorization
            .url()
            .query_pairs()
            .collect::<std::collections::BTreeMap<_, _>>();
        assert_eq!(
            query.get("client_id").map(|value| value.as_ref()),
            Some("client")
        );
        assert_eq!(
            query
                .get("code_challenge_method")
                .map(|value| value.as_ref()),
            Some("S256")
        );
        assert!(
            query
                .get("scope")
                .is_some_and(|scope| scope.contains("offline_access"))
        );
        assert!(
            !format!("{authorization:?}")
                .contains(query.get("state").expect("state query").as_ref())
        );
    }

    #[test]
    fn jwt_claims_supply_account_and_expiration() {
        let token = jwt(serde_json::json!({
            "exp": 1_900_000_000_u64,
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "acct-test"
            }
        }));
        assert_eq!(account_id_from_jwt(&token).as_deref(), Some("acct-test"));
        assert_eq!(
            jwt_expiration_millis(&token).expect("JWT"),
            Some(1_900_000_000_000)
        );
    }

    #[test]
    fn account_id_accepts_top_level_and_organization_claims() {
        let top_level = jwt(serde_json::json!({
            "chatgpt_account_id": "acct-top-level"
        }));
        assert_eq!(
            account_id_from_jwt(&top_level).as_deref(),
            Some("acct-top-level")
        );

        let organization = jwt(serde_json::json!({
            "organizations": [{"id": "org-first"}]
        }));
        assert_eq!(
            account_id_from_jwt(&organization).as_deref(),
            Some("org-first")
        );
    }

    #[test]
    fn residency_ignores_no_constraint_and_accepts_both_claim_locations() {
        let nested = jwt(serde_json::json!({
            "https://api.openai.com/auth": {
                "chatgpt_compute_residency": "us"
            }
        }));
        assert_eq!(residency_from_jwt(&nested).as_deref(), Some("us"));

        let top_level = jwt(serde_json::json!({
            "chatgpt_compute_residency": "eu"
        }));
        assert_eq!(residency_from_jwt(&top_level).as_deref(), Some("eu"));

        let unconstrained = jwt(serde_json::json!({
            "chatgpt_compute_residency": "no_constraint"
        }));
        assert!(residency_from_jwt(&unconstrained).is_none());
    }

    #[test]
    fn device_authorization_debug_redacts_both_one_time_values() {
        let authorization = DeviceAuthorization {
            verification_url: Url::parse("https://auth.example.test/codex/device")
                .expect("verification URL"),
            user_code: Secret::new("USER-CODE-CANARY"),
            device_auth_id: Secret::new("DEVICE-ID-CANARY"),
            interval: Duration::from_secs(5),
        };
        let rendered = format!("{authorization:?}");
        assert!(!rendered.contains("USER-CODE-CANARY"), "{rendered}");
        assert!(!rendered.contains("DEVICE-ID-CANARY"), "{rendered}");
    }

    #[tokio::test]
    async fn refresh_rotates_tokens_and_preserves_enterprise_metadata() {
        #[derive(Clone)]
        struct Capture(Arc<Mutex<Option<String>>>);

        async fn token(
            State(capture): State<Capture>,
            body: String,
        ) -> axum::Json<serde_json::Value> {
            *capture.0.lock().expect("capture") = Some(body);
            axum::Json(serde_json::json!({
                "access_token": jwt(serde_json::json!({
                    "exp": 1_900_000_000_u64,
                    "https://api.openai.com/auth": {
                        "chatgpt_account_id": "acct-refreshed"
                    }
                })),
                "refresh_token": "refresh-next"
            }))
        }

        let capture = Capture(Arc::new(Mutex::new(None)));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let address = listener.local_addr().expect("address");
        let app = Router::new()
            .route(TOKEN_PATH, post(token))
            .with_state(capture.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });

        let client =
            OpenAiOauthClient::with_issuer(&format!("http://{address}"), "client").expect("client");
        let credential = Credential::Oauth {
            refresh: Secret::new("refresh-old"),
            access: Secret::new("access-old"),
            expires: 1,
            account_id: Some("acct-old".to_owned()),
            enterprise_url: Some("https://enterprise.example.test".to_owned()),
        };
        let refreshed = client.refresh(&credential).await.expect("refresh");
        let Credential::Oauth {
            refresh,
            account_id,
            enterprise_url,
            ..
        } = refreshed
        else {
            panic!("expected OAuth");
        };
        assert_eq!(refresh.expose(), "refresh-next");
        assert_eq!(account_id.as_deref(), Some("acct-refreshed"));
        assert_eq!(
            enterprise_url.as_deref(),
            Some("https://enterprise.example.test")
        );
        let form = capture.0.lock().expect("capture").clone().expect("form");
        assert!(form.contains("grant_type=refresh_token"), "{form}");
        assert!(form.contains("refresh_token=refresh-old"), "{form}");
        server.abort();
    }

    #[test]
    fn refresh_decision_includes_a_clock_skew() {
        let credential = Credential::Oauth {
            refresh: Secret::new("r"),
            access: Secret::new("a"),
            expires: now_millis().saturating_add(1_000),
            account_id: None,
            enterprise_url: None,
        };
        assert!(needs_refresh(&credential));
    }
}
