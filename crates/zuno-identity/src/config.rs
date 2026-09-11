use std::collections::BTreeSet;

use crate::OAuth2Authority;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// API admission requirements. These do not grant tool permissions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", deny_unknown_fields)]
pub enum ActorPolicy {
    /// Requires a delegated access token. ID tokens and app-only tokens fail.
    #[serde(rename_all = "camelCase")]
    DelegatedUser {
        required_scopes: BTreeSet<String>,
        #[serde(default)]
        required_roles: BTreeSet<String>,
    },
    /// Requires an app-only token carrying `idtyp=app` and all required roles.
    #[serde(rename_all = "camelCase")]
    Workload { required_roles: BTreeSet<String> },
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid identity configuration: {0}")]
pub struct IdentityConfigError(pub(crate) &'static str);

/// Single-tenant Microsoft public-cloud v2 access-token configuration.
///
/// No issuer, discovery URL, or audience can be supplied by a request. Additional
/// sovereign authorities require explicit adapters; an arbitrary issuer URL is
/// not a supported fallback.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "ConfigDocument", into = "ConfigDocument")]
pub struct EntraConfig {
    pub(crate) tenant_id: String,
    pub(crate) audience: String,
    pub(crate) allowed_clients: BTreeSet<String>,
    pub(crate) actor: ActorPolicy,
    pub(crate) clock_skew_seconds: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ConfigDocument {
    tenant_id: String,
    audience: String,
    allowed_clients: BTreeSet<String>,
    actor: ActorPolicy,
    #[serde(default = "default_clock_skew")]
    clock_skew_seconds: u64,
}

const fn default_clock_skew() -> u64 {
    30
}

impl EntraConfig {
    pub fn new(
        tenant_id: impl Into<String>,
        audience: impl Into<String>,
        allowed_clients: BTreeSet<String>,
        actor: ActorPolicy,
    ) -> Result<Self, IdentityConfigError> {
        Self::try_from(ConfigDocument {
            tenant_id: tenant_id.into(),
            audience: audience.into(),
            allowed_clients,
            actor,
            clock_skew_seconds: default_clock_skew(),
        })
    }

    #[must_use]
    pub fn tenant_id(&self) -> &str {
        &self.tenant_id
    }

    #[must_use]
    pub fn audience(&self) -> &str {
        &self.audience
    }

    #[must_use]
    pub fn issuer(&self) -> String {
        format!("https://login.microsoftonline.com/{}/v2.0", self.tenant_id)
    }

    #[must_use]
    pub fn discovery_url(&self) -> String {
        format!("{}/.well-known/openid-configuration", self.issuer())
    }

    #[must_use]
    pub fn authority(&self) -> OAuth2Authority {
        let mut authority = OAuth2Authority::oidc(self.issuer())
            .expect("validated tenant has a valid fixed Entra authority");
        authority.key_issuer_rule = crate::authority::KeyIssuerRule::Entra;
        authority.allowed_jwks_paths = Some(
            [
                "/common/discovery/v2.0/keys".to_owned(),
                format!("/{}/discovery/v2.0/keys", self.tenant_id),
            ]
            .into(),
        );
        authority
    }
}

impl TryFrom<ConfigDocument> for EntraConfig {
    type Error = IdentityConfigError;

    fn try_from(value: ConfigDocument) -> Result<Self, Self::Error> {
        let tenant_id = canonical_guid(&value.tenant_id)
            .ok_or(IdentityConfigError("tenantId must be a non-nil GUID"))?;
        let audience = canonical_guid(&value.audience).ok_or(IdentityConfigError(
            "audience must be the API application GUID",
        ))?;
        if value.allowed_clients.is_empty() || value.allowed_clients.len() > 128 {
            return Err(IdentityConfigError(
                "allowedClients must contain 1–128 GUIDs",
            ));
        }
        let allowed_clients = value
            .allowed_clients
            .iter()
            .map(|id| {
                canonical_guid(id).ok_or(IdentityConfigError(
                    "each allowed client must be a non-nil GUID",
                ))
            })
            .collect::<Result<BTreeSet<_>, _>>()?;
        match &value.actor {
            ActorPolicy::DelegatedUser {
                required_scopes,
                required_roles,
            } => {
                validate_permissions(required_scopes, true)?;
                validate_permissions(required_roles, false)?;
            }
            ActorPolicy::Workload { required_roles } => {
                validate_permissions(required_roles, true)?;
            }
        }
        if value.clock_skew_seconds > 120 {
            return Err(IdentityConfigError("clockSkewSeconds must be at most 120"));
        }
        Ok(Self {
            tenant_id,
            audience,
            allowed_clients,
            actor: value.actor,
            clock_skew_seconds: value.clock_skew_seconds,
        })
    }
}

impl From<EntraConfig> for ConfigDocument {
    fn from(value: EntraConfig) -> Self {
        Self {
            tenant_id: value.tenant_id,
            audience: value.audience,
            allowed_clients: value.allowed_clients,
            actor: value.actor,
            clock_skew_seconds: value.clock_skew_seconds,
        }
    }
}

pub(crate) fn canonical_guid(value: &str) -> Option<String> {
    // Reject UUID URNs, braces and compact forms at this wire boundary.
    if value.len() != 36 {
        return None;
    }
    let id = Uuid::parse_str(value).ok()?;
    (!id.is_nil()).then(|| id.hyphenated().to_string())
}

fn validate_permissions(
    permissions: &BTreeSet<String>,
    required: bool,
) -> Result<(), IdentityConfigError> {
    if (required && permissions.is_empty())
        || permissions.len() > 128
        || permissions.iter().any(|value| !valid_permission(value))
    {
        return Err(IdentityConfigError(
            "permissions must be bounded, nonempty ASCII scope or role names",
        ));
    }
    Ok(())
}

pub(crate) fn valid_permission(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._:-/".contains(&byte))
}
