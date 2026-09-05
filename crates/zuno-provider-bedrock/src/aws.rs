use std::collections::BTreeMap;
use std::sync::Arc;

use http::{HeaderMap, HeaderName, HeaderValue};
use tokio::sync::OnceCell;
use zuno_aws_auth::{
    AwsAccessKeys, AwsAuthConfig, AwsAuthContext, AwsAuthError, AwsRequestToSign, resolve_region,
};
use zuno_error::ProviderError;

/// Environment variable carrying an Amazon Bedrock API key.
pub const AWS_BEARER_TOKEN_BEDROCK: &str = "AWS_BEARER_TOKEN_BEDROCK";

/// An Amazon Bedrock API key that can only be exposed deliberately.
#[derive(Clone, PartialEq, Eq)]
pub struct BedrockBearerToken(String);

impl BedrockBearerToken {
    /// Wrap one bearer token.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The plaintext token used only at the HTTP authorization boundary.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Whether this token carries no bytes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Debug for BedrockBearerToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("<redacted>")
    }
}

#[derive(Clone)]
pub(crate) struct BedrockRequestAuth {
    provider_id: String,
    config: AwsAuthConfig,
    access_keys: Option<AwsAccessKeys>,
    bearer: Option<BedrockBearerToken>,
    context: Arc<OnceCell<AwsAuthContext>>,
    region: Arc<OnceCell<String>>,
}

impl std::fmt::Debug for BedrockRequestAuth {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BedrockRequestAuth")
            .field("provider_id", &self.provider_id)
            .field(
                "mode",
                &if self.bearer.is_some() {
                    "bearer"
                } else {
                    "sigv4"
                },
            )
            .finish_non_exhaustive()
    }
}

pub(crate) struct AuthorizedRequest {
    pub(crate) url: String,
    pub(crate) headers: HeaderMap,
}

impl BedrockRequestAuth {
    pub(crate) fn new(
        provider_id: impl Into<String>,
        config: AwsAuthConfig,
        access_keys: Option<AwsAccessKeys>,
        bearer: Option<BedrockBearerToken>,
    ) -> Self {
        Self {
            provider_id: provider_id.into(),
            config,
            access_keys,
            bearer: bearer.filter(|token| !token.is_empty()),
            context: Arc::new(OnceCell::new()),
            region: Arc::new(OnceCell::new()),
        }
    }

    #[must_use]
    pub(crate) fn uses_bearer(&self) -> bool {
        self.bearer.is_some()
    }

    pub(crate) async fn region(&self) -> Result<&str, ProviderError> {
        if !self.uses_bearer() {
            return Ok(self.context().await?.region());
        }
        self.region
            .get_or_try_init(|| async {
                resolve_region(&self.config)
                    .await
                    .map_err(|source| map_auth_error(&self.provider_id, source))
            })
            .await
            .map(String::as_str)
    }

    pub(crate) async fn authorize(
        &self,
        request: AwsRequestToSign,
    ) -> Result<AuthorizedRequest, ProviderError> {
        let Some(bearer) = self.bearer.as_ref() else {
            let signed = self
                .context()
                .await?
                .sign(request)
                .await
                .map_err(|source| map_auth_error(&self.provider_id, source))?;
            return Ok(AuthorizedRequest {
                url: signed.url,
                headers: signed.headers,
            });
        };

        let mut headers = request.headers;
        headers.remove(http::header::AUTHORIZATION);
        headers.remove("x-amz-content-sha256");
        headers.remove("x-amz-date");
        headers.remove("x-amz-security-token");
        let mut value =
            HeaderValue::from_str(&format!("Bearer {}", bearer.expose())).map_err(|source| {
                ProviderError::Auth {
                    provider: self.provider_id.clone(),
                    source: Some(Box::new(InvalidBearerToken(source))),
                }
            })?;
        value.set_sensitive(true);
        headers.insert(http::header::AUTHORIZATION, value);
        Ok(AuthorizedRequest {
            url: request.url,
            headers,
        })
    }

    async fn context(&self) -> Result<&AwsAuthContext, ProviderError> {
        let config = self.config.clone();
        let access_keys = self.access_keys.clone();
        self.context
            .get_or_try_init(|| load_context(config, access_keys))
            .await
            .map_err(|source| map_auth_error(&self.provider_id, source))
    }
}

pub(crate) async fn load_context(
    config: AwsAuthConfig,
    access_keys: Option<AwsAccessKeys>,
) -> Result<AwsAuthContext, AwsAuthError> {
    if config.profile.is_some() {
        AwsAuthContext::load_profile(config).await
    } else if let Some(access_keys) = access_keys {
        AwsAuthContext::load_with_access_keys(config, access_keys).await
    } else {
        AwsAuthContext::load(config).await
    }
}

pub(crate) fn header_map(
    headers: &BTreeMap<String, String>,
) -> Result<HeaderMap, AwsRequestHeaderError> {
    headers
        .iter()
        .map(|(name, value)| {
            Ok((
                HeaderName::from_bytes(name.as_bytes())
                    .map_err(|source| AwsRequestHeaderError::Name(name.clone(), source))?,
                HeaderValue::from_str(value)
                    .map_err(|source| AwsRequestHeaderError::Value(name.clone(), source))?,
            ))
        })
        .collect()
}

pub(crate) fn map_auth_error(provider: &str, source: AwsAuthError) -> ProviderError {
    if source.is_retryable() {
        ProviderError::transient(source)
    } else {
        ProviderError::Auth {
            provider: provider.to_owned(),
            source: Some(Box::new(source)),
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("Amazon Bedrock bearer token is not a valid HTTP header value")]
struct InvalidBearerToken(#[source] http::header::InvalidHeaderValue);

#[derive(Debug, thiserror::Error)]
pub(crate) enum AwsRequestHeaderError {
    #[error("invalid Bedrock request header name `{0}`")]
    Name(String, #[source] http::header::InvalidHeaderName),
    #[error("invalid Bedrock request header value for `{0}`")]
    Value(String, #[source] http::header::InvalidHeaderValue),
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use http::{HeaderMap, HeaderValue, Method};

    use super::*;

    fn config() -> AwsAuthConfig {
        AwsAuthConfig {
            profile: None,
            region: Some("us-east-2".to_owned()),
            service: "bedrock".to_owned(),
        }
    }

    #[tokio::test]
    async fn bearer_auth_replaces_every_sigv4_header_without_loading_credentials() {
        let auth = BedrockRequestAuth::new(
            "amazon-bedrock",
            config(),
            None,
            Some(BedrockBearerToken::new("bedrock-api-key")),
        );
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::AUTHORIZATION,
            HeaderValue::from_static("AWS4-HMAC-SHA256 old"),
        );
        headers.insert("x-amz-date", HeaderValue::from_static("20260905T000000Z"));
        headers.insert(
            "x-amz-security-token",
            HeaderValue::from_static("session-token"),
        );
        headers.insert(
            "x-amz-content-sha256",
            HeaderValue::from_static("payload-hash"),
        );

        let authorized = auth
            .authorize(AwsRequestToSign {
                method: Method::POST,
                url: "https://bedrock-runtime.us-east-2.amazonaws.com/model/test/converse"
                    .to_owned(),
                headers,
                body: Bytes::from_static(b"{}"),
            })
            .await
            .expect("bearer authorization");

        assert_eq!(
            authorized.headers[http::header::AUTHORIZATION],
            "Bearer bedrock-api-key"
        );
        assert!(authorized.headers.get("x-amz-date").is_none());
        assert!(authorized.headers.get("x-amz-security-token").is_none());
        assert!(authorized.headers.get("x-amz-content-sha256").is_none());
        assert_eq!(auth.region().await.expect("explicit region"), "us-east-2");
        assert!(!format!("{auth:?}").contains("bedrock-api-key"));
    }
}
