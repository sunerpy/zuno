//! AWS SDK credential resolution and SigV4 signing.
//!
//! Zuno deliberately does not construct an AWS service client for provider traffic:
//! Bedrock HTTP requests continue through `zuno-network`, preserving its proxy, DNS,
//! timeout, and route controls. This crate owns the two AWS-maintained pieces that
//! should not be reimplemented locally: the standard credential provider chain and
//! Signature Version 4.

mod config;
mod discovery;
mod signing;

use std::time::SystemTime;

use aws_credential_types::provider::{ProvideCredentials as _, SharedCredentialsProvider};
use bytes::Bytes;
use http::{HeaderMap, Method};
use thiserror::Error;

pub use discovery::{AwsProfile, discover_aws_profiles, validate_aws_profile};

/// AWS authentication configuration for one provider endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AwsAuthConfig {
    /// Explicit profile. When present, ambient access-key variables cannot override it.
    pub profile: Option<String>,
    /// Explicit region. The AWS SDK resolves profile/environment defaults when absent.
    pub region: Option<String>,
    /// SigV4 service name.
    pub service: String,
}

/// Static AWS access keys supplied by trusted configuration.
#[derive(Clone, PartialEq, Eq)]
pub struct AwsAccessKeys {
    /// AWS access key id.
    pub access_key_id: String,
    /// AWS secret access key.
    pub secret_access_key: String,
    /// Optional temporary-session token.
    pub session_token: Option<String>,
}

impl std::fmt::Debug for AwsAccessKeys {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AwsAccessKeys")
            .field("access_key_id", &"<redacted>")
            .field("secret_access_key", &"<redacted>")
            .field(
                "session_token",
                &self.session_token.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

/// Generic HTTP request shape consumed by SigV4 signing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AwsRequestToSign {
    /// HTTP method.
    pub method: Method,
    /// Absolute request URL.
    pub url: String,
    /// Headers present before signing.
    pub headers: HeaderMap,
    /// Exact body bytes sent after signing.
    pub body: Bytes,
}

/// Signed request URL and headers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AwsSignedRequest {
    /// URL after any signing instruction updates.
    pub url: String,
    /// Complete signed header map.
    pub headers: HeaderMap,
}

/// Credential-loading and SigV4 failures.
#[derive(Debug, Error)]
pub enum AwsAuthError {
    /// Empty service names cannot be signed.
    #[error("AWS service name must not be empty")]
    EmptyService,
    /// A profile-only request omitted the profile.
    #[error("AWS profile must be configured")]
    MissingProfile,
    /// The SDK loaded no credential provider.
    #[error("AWS SDK config did not resolve a credentials provider")]
    MissingCredentialsProvider,
    /// The SDK loaded no region.
    #[error("AWS SDK config did not resolve a region")]
    MissingRegion,
    /// Shared profile files could not be loaded.
    #[error("failed to load AWS profiles: {0}")]
    ProfileLoad(#[from] aws_config::profile::ProfileFileLoadError),
    /// The selected credential provider failed.
    #[error("failed to load AWS credentials: {0}")]
    Credentials(#[from] aws_credential_types::provider::error::CredentialsError),
    /// The request URL could not become an HTTP URI.
    #[error("request URL is not a valid URI: {0}")]
    InvalidUri(#[source] http::uri::InvalidUri),
    /// Signing could not construct the temporary HTTP request.
    #[error("failed to construct HTTP request for signing: {0}")]
    BuildHttpRequest(#[source] http::Error),
    /// SigV4 only signs textual HTTP header values.
    #[error("request contains a non-UTF8 header value: {0}")]
    InvalidHeaderValue(#[source] http::header::ToStrError),
    /// The AWS signer rejected the request shape.
    #[error("failed to build signable request: {0}")]
    SigningRequest(#[source] aws_sigv4::http_request::SigningError),
    /// Signing parameters were incomplete.
    #[error("failed to build SigV4 signing params: {0}")]
    SigningParams(String),
    /// The AWS signer failed.
    #[error("SigV4 signing failed: {0}")]
    SigningFailure(#[source] aws_sigv4::http_request::SigningError),
}

/// Loaded AWS auth context that resolves refreshable credentials and signs requests.
#[derive(Clone)]
pub struct AwsAuthContext {
    credentials_provider: SharedCredentialsProvider,
    region: String,
    service: String,
}

impl std::fmt::Debug for AwsAuthContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AwsAuthContext")
            .field("region", &self.region)
            .field("service", &self.service)
            .finish_non_exhaustive()
    }
}

impl AwsAuthContext {
    /// Load the AWS SDK default chain, optionally pinned to a region/profile.
    pub async fn load(config: AwsAuthConfig) -> Result<Self, AwsAuthError> {
        let sdk_config = config::load_sdk_config(&config).await?;
        let credentials_provider = config::credentials_provider(&sdk_config)?;
        let region = config::resolved_region(&sdk_config)?;
        Ok(Self {
            credentials_provider,
            region,
            service: config.service.trim().to_owned(),
        })
    }

    /// Load the SDK configuration, then replace its credentials with static keys.
    pub async fn load_with_access_keys(
        config: AwsAuthConfig,
        access_keys: AwsAccessKeys,
    ) -> Result<Self, AwsAuthError> {
        let mut context = Self::load(config).await?;
        context.credentials_provider =
            SharedCredentialsProvider::new(aws_credential_types::Credentials::new(
                access_keys.access_key_id,
                access_keys.secret_access_key,
                access_keys.session_token,
                /* expires_after */ None,
                "zuno-configured-bedrock-access-keys",
            ));
        Ok(context)
    }

    /// Load credentials only from the explicitly selected profile.
    pub async fn load_profile(config: AwsAuthConfig) -> Result<Self, AwsAuthError> {
        let profile = config
            .profile
            .as_deref()
            .ok_or(AwsAuthError::MissingProfile)?;
        let credentials_provider = SharedCredentialsProvider::new(
            discovery::profile_credentials_provider(profile, config.region.as_deref()).await,
        );
        let mut context = Self::load(config).await?;
        context.credentials_provider = credentials_provider;
        Ok(context)
    }

    /// Region resolved by the SDK.
    #[must_use]
    pub fn region(&self) -> &str {
        &self.region
    }

    /// SigV4 service name.
    #[must_use]
    pub fn service(&self) -> &str {
        &self.service
    }

    /// Resolve current credentials and sign an outbound request.
    pub async fn sign(&self, request: AwsRequestToSign) -> Result<AwsSignedRequest, AwsAuthError> {
        self.sign_at(request, SystemTime::now()).await
    }

    async fn sign_at(
        &self,
        request: AwsRequestToSign,
        time: SystemTime,
    ) -> Result<AwsSignedRequest, AwsAuthError> {
        let credentials = self.credentials_provider.provide_credentials().await?;
        signing::sign_request(&credentials, &self.region, &self.service, request, time)
    }
}

impl AwsAuthError {
    /// Whether retrying can recover from a temporary credential provider failure.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Credentials(error) => matches!(
                error,
                aws_credential_types::provider::error::CredentialsError::ProviderTimedOut(_)
                    | aws_credential_types::provider::error::CredentialsError::ProviderError(_)
            ),
            Self::EmptyService
            | Self::MissingProfile
            | Self::MissingCredentialsProvider
            | Self::MissingRegion
            | Self::ProfileLoad(_)
            | Self::InvalidUri(_)
            | Self::BuildHttpRequest(_)
            | Self::InvalidHeaderValue(_)
            | Self::SigningRequest(_)
            | Self::SigningParams(_)
            | Self::SigningFailure(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use aws_credential_types::Credentials;

    use super::*;

    fn context(service: &str, session_token: Option<&str>) -> AwsAuthContext {
        AwsAuthContext {
            credentials_provider: SharedCredentialsProvider::new(Credentials::new(
                "AKIDEXAMPLE",
                "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
                session_token.map(str::to_owned),
                /* expires_after */ None,
                "unit-test",
            )),
            region: "us-east-2".to_owned(),
            service: service.to_owned(),
        }
    }

    fn request() -> AwsRequestToSign {
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/json"),
        );
        AwsRequestToSign {
            method: Method::POST,
            url: "https://bedrock-mantle.us-east-2.api.aws/openai/v1/responses".to_owned(),
            headers,
            body: Bytes::from_static(br#"{"model":"openai.gpt-5.6-sol"}"#),
        }
    }

    #[tokio::test]
    async fn aws_signer_uses_the_selected_service_and_preserves_headers() {
        let signed = context("bedrock-mantle", None)
            .sign_at(request(), UNIX_EPOCH + Duration::from_secs(1_700_000_000))
            .await
            .expect("request signs");
        assert_eq!(
            signing::header_value(&signed.headers, "content-type"),
            Some("application/json".to_owned())
        );
        assert!(
            signing::header_value(&signed.headers, "authorization")
                .is_some_and(|value| value.contains("/bedrock-mantle/aws4_request"))
        );
        assert!(signing::header_value(&signed.headers, "x-amz-date").is_some());
    }

    #[tokio::test]
    async fn aws_signer_includes_temporary_session_tokens() {
        let signed = context("bedrock", Some("session-token"))
            .sign_at(request(), UNIX_EPOCH + Duration::from_secs(1_700_000_000))
            .await
            .expect("request signs");
        assert_eq!(
            signing::header_value(&signed.headers, "x-amz-security-token"),
            Some("session-token".to_owned())
        );
    }

    #[tokio::test]
    async fn invalid_auth_configuration_fails_closed() {
        let error = AwsAuthContext::load(AwsAuthConfig {
            profile: None,
            region: None,
            service: " ".to_owned(),
        })
        .await
        .expect_err("empty service must fail");
        assert_eq!(error.to_string(), "AWS service name must not be empty");
    }
}
