use std::collections::BTreeSet;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use zuno_types::identity::{ClientId, PrincipalId, TenantId};

use crate::jwt::{JwtHeaderType, JwtValidator};
use crate::{
    AccessTokenVerifier, IdentityConfigError, IdentityError, KeyCacheOptions, OAuth2Authority,
    OidcKeySource, SigningKeyCache, TokenRejection, VerifiedIdentity, VerifiedIdentityKind,
};

/// Exact, case-sensitive proof supplied by the issuer. This is not an expression
/// evaluator and cannot name email/display-name claims as an identity proof.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaimRequirement {
    pub claim: String,
    pub value: String,
}

impl ClaimRequirement {
    fn validate(&self) -> Result<(), IdentityConfigError> {
        if !claim_name(&self.claim)
            || !bounded_value(&self.value)
            || matches!(
                self.claim.as_str(),
                "email" | "upn" | "preferred_username" | "name"
            )
        {
            return Err(IdentityConfigError("invalid issuer claim requirement"));
        }
        Ok(())
    }

    pub(crate) fn matches(&self, claims: &Value) -> bool {
        claims.get(&self.claim).and_then(Value::as_str) == Some(&self.value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum OAuth2PrincipalKind {
    User,
    Workload,
}

/// Provider-specific admission and subject classification, separate from JWT
/// cryptography. There is no universal OAuth2 "user versus service" claim.
///
/// A deployment must configure an issuer-guaranteed actor marker and restrict
/// scopes/clients. No automatic role, tenant, email or client-name heuristics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "ClaimsPolicyDocument", into = "ClaimsPolicyDocument")]
pub struct OAuth2ClaimsPolicy {
    pub(crate) tenant_id: TenantId,
    pub(crate) audience: String,
    pub(crate) allowed_clients: BTreeSet<String>,
    pub(crate) required_scopes: BTreeSet<String>,
    pub(crate) client_claim: String,
    pub(crate) scope_claim: String,
    pub(crate) principal_kind: OAuth2PrincipalKind,
    pub(crate) actor_claim: ClaimRequirement,
    pub(crate) clock_skew_seconds: u64,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ClaimsPolicyDocument {
    tenant_id: TenantId,
    audience: String,
    allowed_clients: BTreeSet<String>,
    required_scopes: BTreeSet<String>,
    #[serde(default = "client_claim")]
    client_claim: String,
    #[serde(default = "scope_claim")]
    scope_claim: String,
    principal_kind: OAuth2PrincipalKind,
    actor_claim: ClaimRequirement,
    #[serde(default = "clock_skew")]
    clock_skew_seconds: u64,
}

fn client_claim() -> String {
    "client_id".to_owned()
}
fn scope_claim() -> String {
    "scope".to_owned()
}
const fn clock_skew() -> u64 {
    30
}

impl TryFrom<ClaimsPolicyDocument> for OAuth2ClaimsPolicy {
    type Error = IdentityConfigError;

    fn try_from(value: ClaimsPolicyDocument) -> Result<Self, Self::Error> {
        value.actor_claim.validate()?;
        if !bounded_value(&value.audience)
            || value.allowed_clients.is_empty()
            || value.allowed_clients.len() > 128
            || value.allowed_clients.iter().any(|id| !bounded_value(id))
            || value.required_scopes.is_empty()
            || value.required_scopes.len() > 128
            || value
                .required_scopes
                .iter()
                .any(|scope| !valid_scope(scope))
            || !claim_name(&value.client_claim)
            || !claim_name(&value.scope_claim)
            || value.clock_skew_seconds > 120
        {
            return Err(IdentityConfigError(
                "invalid OAuth2 audience, claims, scopes or clients",
            ));
        }
        Ok(Self {
            tenant_id: value.tenant_id,
            audience: value.audience,
            allowed_clients: value.allowed_clients,
            required_scopes: value.required_scopes,
            client_claim: value.client_claim,
            scope_claim: value.scope_claim,
            principal_kind: value.principal_kind,
            actor_claim: value.actor_claim,
            clock_skew_seconds: value.clock_skew_seconds,
        })
    }
}

impl From<OAuth2ClaimsPolicy> for ClaimsPolicyDocument {
    fn from(value: OAuth2ClaimsPolicy) -> Self {
        Self {
            tenant_id: value.tenant_id,
            audience: value.audience,
            allowed_clients: value.allowed_clients,
            required_scopes: value.required_scopes,
            client_claim: value.client_claim,
            scope_claim: value.scope_claim,
            principal_kind: value.principal_kind,
            actor_claim: value.actor_claim,
            clock_skew_seconds: value.clock_skew_seconds,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", deny_unknown_fields)]
pub enum OAuth2JwtProfile {
    /// RFC 9068: at+jwt, client_id, sub, iat, jti and scope.
    Rfc9068,
    /// JWT-form access tokens predating RFC 9068 require an explicit access-token
    /// marker, e.g. token_use=access. A generic JWT is never presumed an access token.
    #[serde(rename_all = "camelCase")]
    Provider {
        access_token_claim: ClaimRequirement,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "JwtConfigDocument", into = "JwtConfigDocument")]
pub struct OAuth2JwtConfig {
    pub(crate) authority: OAuth2Authority,
    pub(crate) claims: OAuth2ClaimsPolicy,
    profile: OAuth2JwtProfile,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct JwtConfigDocument {
    authority: OAuth2Authority,
    claims: OAuth2ClaimsPolicy,
    profile: OAuth2JwtProfile,
}

impl OAuth2JwtConfig {
    pub fn new(
        authority: OAuth2Authority,
        claims: OAuth2ClaimsPolicy,
        profile: OAuth2JwtProfile,
    ) -> Result<Self, IdentityConfigError> {
        Self::try_from(JwtConfigDocument {
            authority,
            claims,
            profile,
        })
    }

    #[must_use]
    pub fn authority(&self) -> &OAuth2Authority {
        &self.authority
    }
}

impl TryFrom<JwtConfigDocument> for OAuth2JwtConfig {
    type Error = IdentityConfigError;
    fn try_from(value: JwtConfigDocument) -> Result<Self, Self::Error> {
        match &value.profile {
            OAuth2JwtProfile::Rfc9068 => {
                if value.claims.client_claim != "client_id" || value.claims.scope_claim != "scope" {
                    return Err(IdentityConfigError(
                        "RFC 9068 uses client_id and scope claims",
                    ));
                }
            }
            OAuth2JwtProfile::Provider { access_token_claim } => access_token_claim.validate()?,
        }
        Ok(Self {
            authority: value.authority,
            claims: value.claims,
            profile: value.profile,
        })
    }
}
impl From<OAuth2JwtConfig> for JwtConfigDocument {
    fn from(value: OAuth2JwtConfig) -> Self {
        Self {
            authority: value.authority,
            claims: value.claims,
            profile: value.profile,
        }
    }
}

/// Generic JWT access-token adapter. RS256 is the implemented signature profile;
/// unsupported algorithms fail before any metadata fetch.
pub struct OAuth2JwtVerifier {
    config: OAuth2JwtConfig,
    jwt: JwtValidator,
}

impl OAuth2JwtVerifier {
    pub fn new(config: OAuth2JwtConfig) -> Result<Self, IdentityError> {
        let cache = Arc::new(SigningKeyCache::new(
            config.authority.clone(),
            Arc::new(OidcKeySource::new(config.authority.clone())?),
            KeyCacheOptions::default(),
        )?);
        Self::with_keys(config, cache)
    }

    pub fn with_keys(
        config: OAuth2JwtConfig,
        keys: Arc<SigningKeyCache>,
    ) -> Result<Self, IdentityError> {
        let jwt = JwtValidator::new(
            config.authority.clone(),
            config.claims.audience.clone(),
            match config.profile {
                OAuth2JwtProfile::Rfc9068 => JwtHeaderType::Rfc9068,
                OAuth2JwtProfile::Provider { .. } => JwtHeaderType::ProviderJwt,
            },
            config.claims.clock_skew_seconds,
            keys,
        )?;
        Ok(Self { config, jwt })
    }
}

#[async_trait]
impl AccessTokenVerifier for OAuth2JwtVerifier {
    async fn verify(&self, token: &str) -> Result<VerifiedIdentity, IdentityError> {
        let claims: Value = self.jwt.validate(token).await?;
        match &self.config.profile {
            OAuth2JwtProfile::Rfc9068 => {
                if claims.get("iat").and_then(Value::as_u64).is_none()
                    || claims
                        .get("jti")
                        .and_then(Value::as_str)
                        .is_none_or(|jti| !bounded_value(jti))
                {
                    return Err(TokenRejection::Claims.into());
                }
            }
            OAuth2JwtProfile::Provider { access_token_claim } => {
                if !access_token_claim.matches(&claims) {
                    return Err(TokenRejection::PrincipalKind.into());
                }
            }
        }
        self.config
            .claims
            .identity(self.config.authority.issuer(), &claims)
    }
}

impl OAuth2ClaimsPolicy {
    /// Called only after cryptographic verification or authenticated introspection.
    pub(crate) fn identity(
        &self,
        issuer: &str,
        claims: &Value,
    ) -> Result<VerifiedIdentity, IdentityError> {
        if claims.get("cnf").is_some_and(|value| !value.is_null()) {
            // Sender-constrained tokens require a separate proof-verification adapter.
            return Err(TokenRejection::UnsupportedToken.into());
        }
        let subject = claims
            .get("sub")
            .and_then(Value::as_str)
            .filter(|value| bounded_value(value))
            .ok_or(TokenRejection::Claims)?;
        let client = claims
            .get(&self.client_claim)
            .and_then(Value::as_str)
            .filter(|value| bounded_value(value))
            .ok_or(TokenRejection::Claims)?;
        if !self.allowed_clients.contains(client) {
            return Err(TokenRejection::Client.into());
        }
        if !self.actor_claim.matches(claims) {
            return Err(TokenRejection::PrincipalKind.into());
        }
        let scope = claims
            .get(&self.scope_claim)
            .and_then(Value::as_str)
            .ok_or(TokenRejection::Permissions)?;
        let scopes: BTreeSet<String> = scope.split(' ').map(str::to_owned).collect();
        if scopes.len() > 128 || scopes.iter().any(|value| !valid_scope(value)) {
            return Err(TokenRejection::Claims.into());
        }
        if !self.required_scopes.is_subset(&scopes) {
            return Err(TokenRejection::Permissions.into());
        }
        let exp = self.validate_times(claims)?;
        let (kind, domain) = match self.principal_kind {
            OAuth2PrincipalKind::User => (VerifiedIdentityKind::DelegatedUser, "oauth2-user-v1"),
            OAuth2PrincipalKind::Workload => (VerifiedIdentityKind::Workload, "oauth2-workload-v1"),
        };
        // Keep the issuer and original opaque subject case-sensitive. Digests use
        // length framing; delimiters or Unicode cannot create identity collisions.
        let principal = PrincipalId::new(framed_id(domain, issuer, subject))
            .map_err(|_| TokenRejection::Claims)?;
        let oauth_client_id = client.to_owned();
        let client = ClientId::new(framed_id("oauth2-client-v1", issuer, client))
            .map_err(|_| TokenRejection::Claims)?;
        Ok(VerifiedIdentity::from_verified(
            issuer.to_owned(),
            self.tenant_id.clone(),
            principal,
            client,
            oauth_client_id,
            kind,
            exp,
        ))
    }

    pub(crate) fn validate_times(&self, claims: &Value) -> Result<u64, IdentityError> {
        let exp = claims
            .get("exp")
            .and_then(Value::as_u64)
            .ok_or(TokenRejection::Claims)?;
        let now = jsonwebtoken::get_current_timestamp();
        if exp.saturating_add(self.clock_skew_seconds) <= now {
            return Err(TokenRejection::Expired.into());
        }
        for key in ["nbf", "iat"] {
            if let Some(value) = claims.get(key) {
                let time = value.as_u64().ok_or(TokenRejection::Claims)?;
                if time > now.saturating_add(self.clock_skew_seconds) {
                    return Err(TokenRejection::NotYetValid.into());
                }
                if time >= exp {
                    return Err(TokenRejection::Claims.into());
                }
            }
        }
        Ok(exp)
    }
}

fn framed_id(domain: &str, issuer: &str, subject: &str) -> String {
    let mut hash = Sha256::new();
    for value in [domain, issuer, subject] {
        hash.update((value.len() as u64).to_be_bytes());
        hash.update(value.as_bytes());
    }
    format!("oauth2-{}", hex::encode(hash.finalize()))
}

fn claim_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
}
fn bounded_value(value: &str) -> bool {
    !value.is_empty() && value.len() <= 1024 && !value.chars().any(char::is_control)
}

// RFC 6749 §3.3 scope-token: %x21 / %x23-5B / %x5D-7E.
fn valid_scope(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|b| b == 0x21 || (0x23..=0x5b).contains(&b) || (0x5d..=0x7e).contains(&b))
}
