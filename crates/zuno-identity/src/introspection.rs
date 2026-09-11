use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use zuno_auth::Secret;

use crate::authority::https_url;
use crate::jwt::validate_token_size;
use crate::{
    AccessTokenVerifier, IdentityConfigError, IdentityError, OAuth2ClaimsPolicy, TokenRejection,
    VerifiedIdentity,
};

const MAX_INTROSPECTION_BYTES: usize = 512 * 1024;

/// Credentials are resolved by the host, never accepted in a request DTO or
/// serialized alongside provider configuration.
#[derive(Clone, Debug)]
pub enum IntrospectionClientAuth {
    ClientSecretBasic {
        client_id: String,
        client_secret: Secret,
    },
    ClientSecretPost {
        client_id: String,
        client_secret: Secret,
    },
}

/// A configured resource-server transport, not a browser/userinfo endpoint.
/// Implementations must authenticate to the configured authorization server and
/// keep token material out of URLs, errors and logs.
#[async_trait]
pub trait TokenIntrospector: Send + Sync {
    async fn introspect(&self, token: &str) -> Result<Value, IdentityError>;
}

pub struct HttpTokenIntrospector {
    client: Client,
    endpoint: Url,
    auth: IntrospectionClientAuth,
}

impl HttpTokenIntrospector {
    pub fn new(endpoint: &str, auth: IntrospectionClientAuth) -> Result<Self, IdentityError> {
        let endpoint = https_url(endpoint)?;
        let (client_id, secret) = match &auth {
            IntrospectionClientAuth::ClientSecretBasic {
                client_id,
                client_secret,
            }
            | IntrospectionClientAuth::ClientSecretPost {
                client_id,
                client_secret,
            } => (client_id, client_secret),
        };
        if client_id.is_empty()
            || client_id.len() > 1024
            || client_id.chars().any(char::is_control)
            || secret.expose().is_empty()
            || secret.expose().len() > 16384
        {
            return Err(IdentityConfigError("invalid introspection client credentials").into());
        }
        let client = zuno_network::client_builder()
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|_| IdentityError::IntrospectionUnavailable)?;
        Ok(Self {
            client,
            endpoint,
            auth,
        })
    }

    fn request(&self, token: &str) -> reqwest::RequestBuilder {
        let request = self.client.post(self.endpoint.clone());
        match &self.auth {
            IntrospectionClientAuth::ClientSecretBasic {
                client_id,
                client_secret,
            } => {
                // RFC 6749 §2.3.1 encodes each credential before HTTP Basic.
                let user: String = url_form_component(client_id);
                let password: String = url_form_component(client_secret.expose());
                request
                    .basic_auth(user, Some(password))
                    .form(&[("token", token), ("token_type_hint", "access_token")])
            }
            IntrospectionClientAuth::ClientSecretPost {
                client_id,
                client_secret,
            } => request.form(&[
                ("token", token),
                ("token_type_hint", "access_token"),
                ("client_id", client_id),
                ("client_secret", client_secret.expose()),
            ]),
        }
    }
}

fn url_form_component(value: &str) -> String {
    // Use the same application/x-www-form-urlencoded encoding as form bodies.
    let encoded = reqwest::Url::parse_with_params("https://encoding.invalid", [("v", value)])
        .expect("fixed encoding URL");
    encoded.query().expect("one query pair")[2..].to_owned()
}

#[async_trait]
impl TokenIntrospector for HttpTokenIntrospector {
    async fn introspect(&self, token: &str) -> Result<Value, IdentityError> {
        validate_token_size(token)?;
        let mut response = self
            .request(token)
            .send()
            .await
            .map_err(|_| IdentityError::IntrospectionUnavailable)?;
        if !response.status().is_success()
            || response
                .content_length()
                .is_some_and(|size| size > MAX_INTROSPECTION_BYTES as u64)
        {
            return Err(IdentityError::IntrospectionUnavailable);
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| IdentityError::IntrospectionUnavailable)?
        {
            if bytes.len().saturating_add(chunk.len()) > MAX_INTROSPECTION_BYTES {
                return Err(IdentityError::IntrospectionUnavailable);
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes).map_err(|_| IdentityError::IntrospectionUnavailable)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "IntrospectionDocument", into = "IntrospectionDocument")]
pub struct OAuth2IntrospectionConfig {
    issuer: String,
    endpoint: String,
    claims: OAuth2ClaimsPolicy,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct IntrospectionDocument {
    issuer: String,
    endpoint: String,
    claims: OAuth2ClaimsPolicy,
}

impl OAuth2IntrospectionConfig {
    pub fn new(
        issuer: impl Into<String>,
        endpoint: impl Into<String>,
        claims: OAuth2ClaimsPolicy,
    ) -> Result<Self, IdentityConfigError> {
        Self::try_from(IntrospectionDocument {
            issuer: issuer.into(),
            endpoint: endpoint.into(),
            claims,
        })
    }
}

impl TryFrom<IntrospectionDocument> for OAuth2IntrospectionConfig {
    type Error = IdentityConfigError;
    fn try_from(value: IntrospectionDocument) -> Result<Self, Self::Error> {
        let issuer = https_url(&value.issuer)?;
        let endpoint = https_url(&value.endpoint)?;
        if issuer.query().is_some() || endpoint.query().is_some() {
            return Err(IdentityConfigError(
                "issuer and introspection endpoints cannot contain queries",
            ));
        }
        Ok(Self {
            issuer: value.issuer,
            endpoint: value.endpoint,
            claims: value.claims,
        })
    }
}
impl From<OAuth2IntrospectionConfig> for IntrospectionDocument {
    fn from(value: OAuth2IntrospectionConfig) -> Self {
        Self {
            issuer: value.issuer,
            endpoint: value.endpoint,
            claims: value.claims,
        }
    }
}

/// RFC 7662 adapter. No positive cache: revocation is checked for every call.
/// An unavailable server never makes a previously active token authoritative.
pub struct OAuth2IntrospectionVerifier {
    config: OAuth2IntrospectionConfig,
    introspector: Arc<dyn TokenIntrospector>,
}

impl OAuth2IntrospectionVerifier {
    pub fn new(
        config: OAuth2IntrospectionConfig,
        auth: IntrospectionClientAuth,
    ) -> Result<Self, IdentityError> {
        let introspector = Arc::new(HttpTokenIntrospector::new(&config.endpoint, auth)?);
        Ok(Self {
            config,
            introspector,
        })
    }

    #[must_use]
    pub fn with_introspector(
        config: OAuth2IntrospectionConfig,
        introspector: Arc<dyn TokenIntrospector>,
    ) -> Self {
        Self {
            config,
            introspector,
        }
    }
}

#[async_trait]
impl AccessTokenVerifier for OAuth2IntrospectionVerifier {
    async fn verify(&self, token: &str) -> Result<VerifiedIdentity, IdentityError> {
        validate_token_size(token)?;
        let claims =
            tokio::time::timeout(Duration::from_secs(10), self.introspector.introspect(token))
                .await
                .map_err(|_| IdentityError::IntrospectionUnavailable)??;
        if claims.get("active").and_then(Value::as_bool) != Some(true) {
            return Err(TokenRejection::InvalidToken.into());
        }
        if let Some(issuer) = claims.get("iss")
            && issuer.as_str() != Some(&self.config.issuer)
        {
            return Err(TokenRejection::Issuer.into());
        }
        let audience_matches = match claims.get("aud") {
            Some(Value::String(audience)) => audience == &self.config.claims.audience,
            Some(Value::Array(audiences)) => {
                audiences.len() <= 32
                    && audiences.iter().all(Value::is_string)
                    && audiences
                        .iter()
                        .any(|audience| audience.as_str() == Some(&self.config.claims.audience))
            }
            _ => false,
        };
        if !audience_matches {
            return Err(TokenRejection::Audience.into());
        }
        if claims.get("token_type").is_some_and(|kind| {
            kind.as_str()
                .is_none_or(|s| !s.eq_ignore_ascii_case("Bearer"))
        }) {
            return Err(TokenRejection::UnsupportedToken.into());
        }
        self.config.claims.identity(&self.config.issuer, &claims)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{Engine, engine::general_purpose::STANDARD};

    #[test]
    fn introspection_authentication_uses_encoded_credentials_and_keeps_tokens_out_of_urls() {
        let token = "access-token-canary";
        let auth = IntrospectionClientAuth::ClientSecretBasic {
            client_id: "client:with spaces".to_owned(),
            client_secret: Secret::new("secret:with+symbols"),
        };
        let client =
            HttpTokenIntrospector::new("https://issuer.test/introspect", auth.clone()).unwrap();
        let request = client.request(token).build().unwrap();
        let header = request
            .headers()
            .get("authorization")
            .unwrap()
            .to_str()
            .unwrap();
        let encoded = String::from_utf8(
            STANDARD
                .decode(header.strip_prefix("Basic ").unwrap())
                .unwrap(),
        )
        .unwrap();
        assert_eq!(encoded, "client%3Awith+spaces:secret%3Awith%2Bsymbols");
        assert!(!request.url().as_str().contains(token));
        assert!(!format!("{auth:?}").contains("secret:with+symbols"));
        let body = std::str::from_utf8(request.body().unwrap().as_bytes().unwrap()).unwrap();
        assert!(body.contains("token=access-token-canary"));
        assert!(body.contains("token_type_hint=access_token"));
        assert!(!body.contains("secret"));
        assert!(HttpTokenIntrospector::new("http://issuer.test/introspect", auth).is_err());
    }
}
