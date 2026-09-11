//! Authorization-code login for a host-owned OIDC provider.
//!
//! Browser transaction storage consumes an attempt before calling `complete`.
//! A lost token-exchange response requires a fresh login, never a POST replay.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use reqwest::Url;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use zuno_auth::Secret;

use crate::{
    AccessTokenVerifier, IdentityConfigError, IdentityError, OAuth2Authority, OidcIdTokenVerifier,
    OidcLoginOptions, SigningKeyCache, VerifiedIdentity, VerifiedIdentityKind,
};

#[derive(Debug, thiserror::Error)]
pub enum LoginError {
    #[error("the login transaction is invalid or expired")]
    Transaction,
    #[error("the identity provider did not complete the code exchange")]
    Exchange,
    #[error("the login state service is temporarily unavailable")]
    Unavailable,
    #[error(transparent)]
    Identity(#[from] IdentityError),
    #[error(transparent)]
    Configuration(#[from] IdentityConfigError),
}

/// Endpoints are accepted only from administrator-selected, exact-issuer metadata.
#[derive(Clone)]
pub struct OidcLoginConfig {
    authority: OAuth2Authority,
    client_id: String,
    authorization_endpoint: Url,
    token_endpoint: Url,
    redirect_uri: Url,
    scopes: BTreeSet<String>,
    options: OidcLoginOptions,
}

impl OidcLoginConfig {
    pub fn from_metadata(
        authority: OAuth2Authority,
        client_id: String,
        redirect_uri: &str,
        scopes: BTreeSet<String>,
        metadata: &[u8],
    ) -> Result<Self, LoginError> {
        Self::from_metadata_with_options(
            authority,
            client_id,
            redirect_uri,
            scopes,
            metadata,
            OidcLoginOptions::default(),
        )
    }

    pub fn from_metadata_with_options(
        authority: OAuth2Authority,
        client_id: String,
        redirect_uri: &str,
        scopes: BTreeSet<String>,
        metadata: &[u8],
        options: OidcLoginOptions,
    ) -> Result<Self, LoginError> {
        if metadata.len() > 512 * 1024
            || client_id.is_empty()
            || client_id.len() > 256
            || client_id.chars().any(char::is_control)
            || !scopes.contains("openid")
            || scopes.len() > 128
            || scopes.iter().any(|scope| {
                scope.is_empty()
                    || scope.len() > 1024
                    || !scope
                        .bytes()
                        .all(|byte| matches!(byte, 0x21 | 0x23..=0x5b | 0x5d..=0x7e))
            })
        {
            return Err(IdentityConfigError("invalid OIDC login configuration").into());
        }
        let metadata: ProviderMetadata =
            serde_json::from_slice(metadata).map_err(|_| LoginError::Exchange)?;
        if metadata.issuer != authority.issuer()
            || !metadata
                .response_types_supported
                .iter()
                .any(|value| value == "code")
        {
            return Err(IdentityConfigError(
                "provider metadata does not support the configured code-flow issuer",
            )
            .into());
        }
        let mut origins = options.additional_endpoint_origins.clone();
        origins.insert(
            crate::authority::https_url(authority.issuer())?
                .origin()
                .ascii_serialization(),
        );
        let authorization_endpoint = crate::authority::https_url(&metadata.authorization_endpoint)?;
        let token_endpoint = crate::authority::https_url(&metadata.token_endpoint)?;
        let redirect_uri = crate::authority::https_url(redirect_uri)?;
        if !origins.contains(&authorization_endpoint.origin().ascii_serialization())
            || !origins.contains(&token_endpoint.origin().ascii_serialization())
            || authorization_endpoint.query().is_some()
            || token_endpoint.query().is_some()
            || redirect_uri.query().is_some()
        {
            return Err(IdentityConfigError(
                "OIDC endpoints must use their trusted origin without query overrides",
            )
            .into());
        }
        Ok(Self {
            authority,
            client_id,
            authorization_endpoint,
            token_endpoint,
            redirect_uri,
            scopes,
            options,
        })
    }

    fn fingerprint(&self) -> [u8; 32] {
        let mut digest = Sha256::new();
        for value in [
            self.authority.issuer(),
            &self.client_id,
            self.authorization_endpoint.as_str(),
            self.token_endpoint.as_str(),
            self.redirect_uri.as_str(),
        ]
        .into_iter()
        .chain(self.scopes.iter().map(String::as_str))
        {
            digest.update((value.len() as u64).to_be_bytes());
            digest.update(value.as_bytes());
        }
        digest.update(serde_json::to_vec(&self.options).expect("login options serialize"));
        digest.finalize().into()
    }
}

#[derive(Deserialize)]
struct ProviderMetadata {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    response_types_supported: Vec<String>,
}

/// Secret material is held by the server, never placed in a client DTO.
pub struct LoginAttempt {
    pub(crate) state: Secret,
    pub(crate) nonce: Secret,
    pub(crate) verifier: Secret,
    pub(crate) browser_binding: [u8; 32],
    pub(crate) configuration: [u8; 32],
    pub(crate) expires_at: u64,
}

impl LoginAttempt {
    pub fn state(&self) -> &str {
        self.state.expose()
    }
    pub fn expires_at_seconds(&self) -> u64 {
        self.expires_at
    }
}

pub struct LoginStart {
    pub authorization_url: Url,
    pub attempt: LoginAttempt,
}

pub struct CodeRequest<'a> {
    pub code: &'a str,
    pub verifier: &'a str,
    pub redirect_uri: &'a Url,
    pub client_id: &'a str,
}

pub struct LoginTokens {
    pub access_token: Secret,
    pub id_token: Secret,
}

/// The BFF consumes its stored transaction before invoking this transport.
#[async_trait]
pub trait AuthorizationCodeExchange: Send + Sync {
    async fn exchange(
        &self,
        endpoint: &Url,
        request: CodeRequest<'_>,
    ) -> Result<LoginTokens, LoginError>;
}

#[derive(Clone, Copy)]
pub enum CodeClientAuthMethod {
    Basic,
    Post,
}

pub struct HttpCodeExchange {
    client: reqwest::Client,
    method: CodeClientAuthMethod,
    client_secret: Secret,
}

impl HttpCodeExchange {
    pub fn new(method: CodeClientAuthMethod, client_secret: Secret) -> Result<Self, LoginError> {
        Self::with_root_certificate(method, client_secret, None)
    }

    /// A trust root is supplied by deployment configuration, never by a request.
    pub fn with_root_certificate(
        method: CodeClientAuthMethod,
        client_secret: Secret,
        root_pem: Option<&[u8]>,
    ) -> Result<Self, LoginError> {
        if client_secret.expose().is_empty() || client_secret.expose().len() > 16_384 {
            return Err(IdentityConfigError("invalid OIDC client credential").into());
        }
        let mut builder = zuno_network::client_builder()
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(15));
        if let Some(pem) = root_pem {
            if pem.len() > 65536 {
                return Err(LoginError::Configuration(IdentityConfigError(
                    "identity trust root is too large",
                )));
            }
            builder = builder.add_root_certificate(reqwest::Certificate::from_pem(pem).map_err(
                |_| LoginError::Configuration(IdentityConfigError("invalid identity trust root")),
            )?);
        }
        let client = builder.build().map_err(|_| LoginError::Exchange)?;
        Ok(Self {
            client,
            method,
            client_secret,
        })
    }
}

#[async_trait]
impl AuthorizationCodeExchange for HttpCodeExchange {
    async fn exchange(
        &self,
        endpoint: &Url,
        request: CodeRequest<'_>,
    ) -> Result<LoginTokens, LoginError> {
        let mut form = vec![
            ("grant_type", "authorization_code"),
            ("code", request.code),
            ("code_verifier", request.verifier),
            ("redirect_uri", request.redirect_uri.as_str()),
        ];
        let mut call = self.client.post(endpoint.clone());
        match self.method {
            CodeClientAuthMethod::Basic => {
                fn encoded(value: &str) -> String {
                    Url::parse_with_params("https://encoding.invalid", [("v", value)])
                        .expect("fixed encoding URL")
                        .query()
                        .expect("one pair")[2..]
                        .to_owned()
                }
                call = call.basic_auth(
                    encoded(request.client_id),
                    Some(encoded(self.client_secret.expose())),
                );
            }
            CodeClientAuthMethod::Post => {
                form.push(("client_id", request.client_id));
                form.push(("client_secret", self.client_secret.expose()));
            }
        }
        let mut response = call
            .form(&form)
            .send()
            .await
            .map_err(|_| LoginError::Exchange)?;
        if !response.status().is_success()
            || response
                .content_length()
                .is_some_and(|size| size > 128 * 1024)
        {
            return Err(LoginError::Exchange);
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| LoginError::Exchange)? {
            if bytes.len().saturating_add(chunk.len()) > 128 * 1024 {
                return Err(LoginError::Exchange);
            }
            bytes.extend_from_slice(&chunk);
        }
        #[derive(Deserialize)]
        struct Response {
            access_token: String,
            id_token: String,
            token_type: String,
        }
        let response: Response =
            serde_json::from_slice(&bytes).map_err(|_| LoginError::Exchange)?;
        if !response.token_type.eq_ignore_ascii_case("Bearer")
            || response.access_token.is_empty()
            || response.id_token.is_empty()
        {
            return Err(LoginError::Exchange);
        }
        Ok(LoginTokens {
            access_token: Secret::new(response.access_token),
            id_token: Secret::new(response.id_token),
        })
    }
}

pub struct OidcLoginClient {
    config: OidcLoginConfig,
    transport: Arc<dyn AuthorizationCodeExchange>,
    id_tokens: OidcIdTokenVerifier,
    access_tokens: Arc<dyn AccessTokenVerifier>,
}

pub struct VerifiedLogin {
    pub identity: VerifiedIdentity,
    pub expires_at_seconds: u64,
}

impl OidcLoginClient {
    pub fn issuer(&self) -> &str {
        self.config.authority.issuer()
    }
    pub fn client_id(&self) -> &str {
        &self.config.client_id
    }
    pub fn redirect_uri(&self) -> &Url {
        &self.config.redirect_uri
    }

    pub fn new(
        config: OidcLoginConfig,
        keys: Arc<SigningKeyCache>,
        transport: Arc<dyn AuthorizationCodeExchange>,
        access_tokens: Arc<dyn AccessTokenVerifier>,
    ) -> Result<Self, LoginError> {
        let id_tokens = OidcIdTokenVerifier::new(
            config.authority.clone(),
            config.client_id.clone(),
            config.options.clock_skew_seconds,
            keys,
        )?;
        Ok(Self {
            config,
            transport,
            id_tokens,
            access_tokens,
        })
    }

    pub fn begin(&self, browser_binding: &str) -> Result<LoginStart, LoginError> {
        if browser_binding.len() < 32 || browser_binding.len() > 256 {
            return Err(LoginError::Transaction);
        }
        fn random() -> Result<Secret, LoginError> {
            use aws_lc_rs::rand::SecureRandom as _;
            let mut bytes = [0u8; 32];
            aws_lc_rs::rand::SystemRandom::new()
                .fill(&mut bytes)
                .map_err(|_| LoginError::Transaction)?;
            Ok(Secret::new(URL_SAFE_NO_PAD.encode(bytes)))
        }
        let attempt = LoginAttempt {
            state: random()?,
            nonce: random()?,
            verifier: random()?,
            browser_binding: Sha256::digest(browser_binding.as_bytes()).into(),
            configuration: self.config.fingerprint(),
            expires_at: jsonwebtoken::get_current_timestamp()
                .saturating_add(self.config.options.transaction_lifetime_seconds),
        };
        let challenge =
            URL_SAFE_NO_PAD.encode(Sha256::digest(attempt.verifier.expose().as_bytes()));
        let mut url = self.config.authorization_endpoint.clone();
        url.query_pairs_mut().extend_pairs([
            ("response_type", "code"),
            ("client_id", self.config.client_id.as_str()),
            ("redirect_uri", self.config.redirect_uri.as_str()),
            (
                "scope",
                &self
                    .config
                    .scopes
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(" "),
            ),
            ("state", attempt.state.expose()),
            ("nonce", attempt.nonce.expose()),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
        ]);
        if let Some(age) = self.config.options.max_authentication_age_seconds {
            url.query_pairs_mut()
                .append_pair("max_age", &age.to_string());
        }
        Ok(LoginStart {
            authorization_url: url,
            attempt,
        })
    }

    pub async fn complete(
        &self,
        attempt: LoginAttempt,
        state: &str,
        browser_binding: &str,
        code: &str,
    ) -> Result<VerifiedLogin, LoginError> {
        let now = jsonwebtoken::get_current_timestamp();
        if now >= attempt.expires_at
            || code.is_empty()
            || code.len() > 16_384
            || code.chars().any(char::is_control)
            || attempt.configuration != self.config.fingerprint()
            || aws_lc_rs::constant_time::verify_slices_are_equal(
                state.as_bytes(),
                attempt.state.expose().as_bytes(),
            )
            .is_err()
            || aws_lc_rs::constant_time::verify_slices_are_equal(
                &Sha256::digest(browser_binding.as_bytes()),
                &attempt.browser_binding,
            )
            .is_err()
        {
            return Err(LoginError::Transaction);
        }
        let tokens = self
            .transport
            .exchange(
                &self.config.token_endpoint,
                CodeRequest {
                    code,
                    verifier: attempt.verifier.expose(),
                    redirect_uri: &self.config.redirect_uri,
                    client_id: &self.config.client_id,
                },
            )
            .await?;
        let authentication = self
            .id_tokens
            .verify(
                tokens.id_token.expose(),
                attempt.nonce.expose(),
                Some(tokens.access_token.expose()),
                self.config.options.max_authentication_age_seconds,
            )
            .await?;
        let identity = self
            .access_tokens
            .verify(tokens.access_token.expose())
            .await?;
        if identity.issuer() != authentication.issuer()
            || identity.oauth_client_id() != self.config.client_id
            || identity.kind() != VerifiedIdentityKind::DelegatedUser
        {
            return Err(LoginError::Transaction);
        }
        let now = jsonwebtoken::get_current_timestamp();
        let expires_at_seconds = identity
            .expires_at_seconds()
            .min(authentication.expires_at_seconds())
            .min(now.saturating_add(self.config.options.session_lifetime_seconds));
        if expires_at_seconds <= now {
            return Err(LoginError::Transaction);
        }
        Ok(VerifiedLogin {
            identity,
            expires_at_seconds,
        })
    }
}
