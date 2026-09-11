use std::collections::BTreeSet;
use std::num::NonZeroU64;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use zuno_types::identity::{ClientId, PrincipalId, PrincipalKind, PrincipalScope, TenantId};

use crate::config::{canonical_guid, valid_permission};
use crate::jwt::{JwtHeaderType, JwtValidator};
use crate::{ActorPolicy, EntraConfig, KeyCacheOptions, OidcKeySource, SigningKeyCache};

/// Safe audit categories; errors never retain or render a bearer token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TokenRejection {
    #[error("invalid access token")]
    InvalidToken,
    #[error("unsupported token algorithm or header")]
    UnsupportedToken,
    #[error("unknown signing key")]
    UnknownKey,
    #[error("token issuer is not trusted")]
    Issuer,
    #[error("token is intended for another API")]
    Audience,
    #[error("token has expired")]
    Expired,
    #[error("token is not yet valid")]
    NotYetValid,
    #[error("invalid identity claims")]
    Claims,
    #[error("calling application is not allowed")]
    Client,
    #[error("token has the wrong principal kind")]
    PrincipalKind,
    #[error("required delegated scope or application role is missing")]
    Permissions,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IdentityError {
    #[error(transparent)]
    Rejected(#[from] TokenRejection),
    #[error("trusted signing keys are temporarily unavailable")]
    KeysUnavailable,
    #[error("the token introspection service is temporarily unavailable")]
    IntrospectionUnavailable,
    #[error(transparent)]
    Configuration(#[from] crate::IdentityConfigError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifiedIdentityKind {
    DelegatedUser,
    Workload,
}

/// Constructed only after signature, audience, tenant and actor validation.
///
/// Intentionally neither deserializable nor constructible from client DTOs.
/// Scopes and roles below establish API admission, not tool/Memory authority.
#[derive(Debug, Clone)]
pub struct VerifiedIdentity {
    tenant_id: TenantId,
    principal_id: PrincipalId,
    client_id: ClientId,
    kind: VerifiedIdentityKind,
    expires_at_seconds: u64,
}

impl VerifiedIdentity {
    pub(crate) fn from_verified(
        tenant_id: TenantId,
        principal_id: PrincipalId,
        client_id: ClientId,
        kind: VerifiedIdentityKind,
        expires_at_seconds: u64,
    ) -> Self {
        Self {
            tenant_id,
            principal_id,
            client_id,
            kind,
            expires_at_seconds,
        }
    }

    #[must_use]
    pub fn tenant_id(&self) -> &TenantId {
        &self.tenant_id
    }

    #[must_use]
    pub fn principal_id(&self) -> &PrincipalId {
        &self.principal_id
    }

    #[must_use]
    pub fn client_id(&self) -> &ClientId {
        &self.client_id
    }

    #[must_use]
    pub const fn kind(&self) -> VerifiedIdentityKind {
        self.kind
    }

    #[must_use]
    pub const fn expires_at_seconds(&self) -> u64 {
        self.expires_at_seconds
    }

    /// Capture attribution only after the host obtains this current policy
    /// revision from its authority service. Never read the revision from a JWT.
    #[must_use]
    pub fn attribution(&self, current_policy_revision: NonZeroU64) -> PrincipalScope {
        PrincipalScope::new(
            self.tenant_id.clone(),
            self.principal_id.clone(),
            match self.kind {
                VerifiedIdentityKind::DelegatedUser => PrincipalKind::User,
                VerifiedIdentityKind::Workload => PrincipalKind::Workload,
            },
            Some(self.client_id.clone()),
            current_policy_revision,
        )
    }
}

#[async_trait]
pub trait AccessTokenVerifier: Send + Sync {
    async fn verify(&self, access_token: &str) -> Result<VerifiedIdentity, IdentityError>;
}

pub struct EntraVerifier {
    config: EntraConfig,
    jwt: JwtValidator,
}

impl EntraVerifier {
    pub fn new(config: EntraConfig) -> Result<Self, IdentityError> {
        let source = Arc::new(OidcKeySource::new(config.authority())?);
        let keys = Arc::new(SigningKeyCache::new(
            config.authority(),
            source,
            KeyCacheOptions::default(),
        )?);
        Self::with_keys(config, keys)
    }

    /// Share a validated tenant cache across user and workload API policies.
    pub fn with_keys(
        config: EntraConfig,
        keys: Arc<SigningKeyCache>,
    ) -> Result<Self, IdentityError> {
        let jwt = JwtValidator::new(
            config.authority(),
            config.audience.clone(),
            JwtHeaderType::ProviderJwt,
            config.clock_skew_seconds,
            keys,
        )?;
        Ok(Self { config, jwt })
    }
}

#[derive(Deserialize)]
struct AccessClaims {
    iss: String,
    aud: String,
    tid: String,
    oid: String,
    azp: String,
    sub: String,
    ver: String,
    exp: u64,
    nbf: u64,
    iat: u64,
    scp: Option<String>,
    #[serde(default)]
    roles: Vec<String>,
    idtyp: Option<String>,
    cnf: Option<serde_json::Value>,
}

#[async_trait]
impl AccessTokenVerifier for EntraVerifier {
    async fn verify(&self, token: &str) -> Result<VerifiedIdentity, IdentityError> {
        let claims: AccessClaims = self.jwt.validate(token).await?;
        if claims.cnf.is_some() {
            return Err(TokenRejection::UnsupportedToken.into());
        }
        let tenant = canonical_guid(&claims.tid).ok_or(TokenRejection::Claims)?;
        let subject = canonical_guid(&claims.oid).ok_or(TokenRejection::Claims)?;
        let client = canonical_guid(&claims.azp).ok_or(TokenRejection::Claims)?;
        if tenant != self.config.tenant_id
            || claims.iss != self.config.issuer()
            || claims.aud != self.config.audience
            || claims.ver != "2.0"
            || claims.sub.is_empty()
            || claims.sub.len() > 256
            || claims.exp <= claims.nbf
            || claims.exp <= claims.iat
            || claims.iat
                > jsonwebtoken::get_current_timestamp()
                    .saturating_add(self.config.clock_skew_seconds)
        {
            return Err(TokenRejection::Claims.into());
        }
        if !self.config.allowed_clients.contains(&client) {
            return Err(TokenRejection::Client.into());
        }
        if claims.roles.len() > 128 || claims.roles.iter().any(|value| !valid_permission(value)) {
            return Err(TokenRejection::Claims.into());
        }
        let roles: BTreeSet<_> = claims.roles.into_iter().collect();
        let kind = match &self.config.actor {
            ActorPolicy::DelegatedUser {
                required_scopes,
                required_roles,
            } => {
                if claims.idtyp.as_deref().is_some_and(|kind| kind != "user") {
                    return Err(TokenRejection::PrincipalKind.into());
                }
                let scp = claims.scp.ok_or(TokenRejection::PrincipalKind)?;
                let scopes: BTreeSet<String> = scp.split(' ').map(str::to_owned).collect();
                if scopes.len() > 128 || scopes.iter().any(|scope| !valid_permission(scope)) {
                    return Err(TokenRejection::Claims.into());
                }
                if !required_scopes.is_subset(&scopes) || !required_roles.is_subset(&roles) {
                    return Err(TokenRejection::Permissions.into());
                }
                VerifiedIdentityKind::DelegatedUser
            }
            ActorPolicy::Workload { required_roles } => {
                if claims.idtyp.as_deref() != Some("app") || claims.scp.is_some() {
                    return Err(TokenRejection::PrincipalKind.into());
                }
                if !required_roles.is_subset(&roles) {
                    return Err(TokenRejection::Permissions.into());
                }
                VerifiedIdentityKind::Workload
            }
        };
        Ok(VerifiedIdentity {
            tenant_id: TenantId::new(tenant).map_err(|_| TokenRejection::Claims)?,
            principal_id: PrincipalId::new(subject).map_err(|_| TokenRejection::Claims)?,
            client_id: ClientId::new(client).map_err(|_| TokenRejection::Claims)?,
            kind,
            expires_at_seconds: claims.exp,
        })
    }
}
