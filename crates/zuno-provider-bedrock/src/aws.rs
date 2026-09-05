use std::collections::BTreeMap;

use http::{HeaderMap, HeaderName, HeaderValue};
use zuno_aws_auth::{AwsAccessKeys, AwsAuthConfig, AwsAuthContext, AwsAuthError};
use zuno_error::ProviderError;

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
pub(crate) enum AwsRequestHeaderError {
    #[error("invalid Bedrock request header name `{0}`")]
    Name(String, #[source] http::header::InvalidHeaderName),
    #[error("invalid Bedrock request header value for `{0}`")]
    Value(String, #[source] http::header::InvalidHeaderValue),
}
