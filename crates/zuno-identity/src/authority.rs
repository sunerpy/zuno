use std::collections::BTreeSet;

use reqwest::Url;
use serde::{Deserialize, Serialize};

use crate::{IdentityConfigError, IdentityError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum KeyIssuerRule {
    Optional,
    Entra,
}

/// A deployment-owned issuer and explicit key-fetch trust boundary.
///
/// HTTPS is mandatory. A trusted administrator can configure an internal IdP;
/// no URL from a bearer token is ever used. An issuer may use OIDC discovery or
/// an explicitly configured JWKS endpoint when it does not implement OIDC.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "AuthorityDocument", into = "AuthorityDocument")]
pub struct OAuth2Authority {
    pub(crate) issuer: String,
    pub(crate) jwks_url: Option<String>,
    pub(crate) allowed_jwks_origins: BTreeSet<String>,
    pub(crate) key_issuer_rule: KeyIssuerRule,
    pub(crate) allowed_jwks_paths: Option<BTreeSet<String>>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AuthorityDocument {
    issuer: String,
    #[serde(default)]
    jwks_url: Option<String>,
    #[serde(default)]
    additional_jwks_origins: BTreeSet<String>,
}

impl OAuth2Authority {
    pub fn oidc(issuer: impl Into<String>) -> Result<Self, IdentityConfigError> {
        Self::try_from(AuthorityDocument {
            issuer: issuer.into(),
            jwks_url: None,
            additional_jwks_origins: BTreeSet::new(),
        })
    }

    pub fn with_jwks(
        issuer: impl Into<String>,
        jwks_url: impl Into<String>,
    ) -> Result<Self, IdentityConfigError> {
        Self::try_from(AuthorityDocument {
            issuer: issuer.into(),
            jwks_url: Some(jwks_url.into()),
            additional_jwks_origins: BTreeSet::new(),
        })
    }

    #[must_use]
    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    #[must_use]
    pub fn discovery_url(&self) -> String {
        format!(
            "{}/.well-known/openid-configuration",
            self.issuer.trim_end_matches('/')
        )
    }

    pub(crate) fn check_jwks_url(&self, value: &str) -> Result<Url, IdentityError> {
        let url = https_url(value).map_err(|_| IdentityError::KeysUnavailable)?;
        if !self
            .allowed_jwks_origins
            .contains(&url.origin().ascii_serialization())
            || self
                .allowed_jwks_paths
                .as_ref()
                .is_some_and(|paths| !paths.contains(url.path()) || url.query().is_some())
        {
            return Err(IdentityError::KeysUnavailable);
        }
        Ok(url)
    }
}

impl TryFrom<AuthorityDocument> for OAuth2Authority {
    type Error = IdentityConfigError;

    fn try_from(value: AuthorityDocument) -> Result<Self, Self::Error> {
        let issuer_url = https_url(&value.issuer)?;
        if issuer_url.query().is_some() || value.additional_jwks_origins.len() > 8 {
            return Err(IdentityConfigError("invalid issuer or key origin count"));
        }
        let mut origins = BTreeSet::from([issuer_url.origin().ascii_serialization()]);
        for origin in value.additional_jwks_origins {
            let url = https_url(&origin)?;
            if url.path() != "/" || url.query().is_some() {
                return Err(IdentityConfigError(
                    "key origins cannot contain paths or queries",
                ));
            }
            origins.insert(url.origin().ascii_serialization());
        }
        let authority = Self {
            issuer: value.issuer,
            jwks_url: value.jwks_url,
            allowed_jwks_origins: origins,
            key_issuer_rule: KeyIssuerRule::Optional,
            allowed_jwks_paths: None,
        };
        if let Some(url) = &authority.jwks_url {
            authority
                .check_jwks_url(url)
                .map_err(|_| IdentityConfigError("JWKS endpoint is outside trusted origins"))?;
        }
        Ok(authority)
    }
}

impl From<OAuth2Authority> for AuthorityDocument {
    fn from(value: OAuth2Authority) -> Self {
        Self {
            issuer: value.issuer,
            jwks_url: value.jwks_url,
            additional_jwks_origins: value.allowed_jwks_origins,
        }
    }
}

pub(crate) fn https_url(value: &str) -> Result<Url, IdentityConfigError> {
    if value.len() > 2048 || value.chars().any(char::is_whitespace) {
        return Err(IdentityConfigError("invalid HTTPS endpoint"));
    }
    let url = Url::parse(value).map_err(|_| IdentityConfigError("invalid HTTPS endpoint"))?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(IdentityConfigError(
            "endpoints require HTTPS without userinfo or fragments",
        ));
    }
    Ok(url)
}
