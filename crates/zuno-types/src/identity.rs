//! Logical ownership shared by storage, execution, and permission boundaries.
//!
//! These values contain identifiers, never credentials or authorization grants.
//! Authenticating a caller and checking current policy remain the host's job.
//! A serialized scope received from a client is not proof of its identity.

use std::borrow::Cow;
use std::fmt;
use std::num::NonZeroU64;

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Serialize};

const ID_PATTERN: &str = "^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$";

/// An identity did not satisfy its boundary representation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidIdentity {
    kind: &'static str,
}

impl fmt::Display for InvalidIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} must be a 1–128 byte opaque identifier",
            self.kind
        )
    }
}

impl std::error::Error for InvalidIdentity {}

fn valid_id(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 128
        && bytes[0].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._:-".contains(byte))
}

macro_rules! identity_id {
    ($name:ident, $description:literal) => {
        #[doc = $description]
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, InvalidIdentity> {
                let value = value.into();
                if valid_id(&value) {
                    Ok(Self(value))
                } else {
                    Err(InvalidIdentity {
                        kind: stringify!($name),
                    })
                }
            }

            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<String> for $name {
            type Error = InvalidIdentity;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }

        impl JsonSchema for $name {
            fn schema_name() -> Cow<'static, str> {
                stringify!($name).into()
            }

            fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
                json_schema!({
                    "type": "string",
                    "minLength": 1,
                    "maxLength": 128,
                    "pattern": ID_PATTERN
                })
            }
        }
    };
}

identity_id!(
    TenantId,
    "An organization namespace; never inferred from a directory."
);
identity_id!(
    PrincipalId,
    "A durable user, application, or workload subject."
);
identity_id!(
    ClientId,
    "An authenticated calling application, independent of a tool name."
);
identity_id!(
    ProjectId,
    "A logical project identity rather than a checkout path."
);
identity_id!(
    WorkspaceId,
    "An owned workspace identity rather than a machine path."
);
identity_id!(GroupId, "An organization-managed group identity.");
identity_id!(
    SessionId,
    "A stable session identity, independent of an executor."
);
identity_id!(RequestId, "A caller's stable idempotency key.");
identity_id!(InputId, "A durable inbox input identity.");
identity_id!(
    JobId,
    "A logical job identity that survives worker changes."
);
identity_id!(TurnId, "The model/tool turn advanced by a job.");
identity_id!(
    WorkerInstanceId,
    "One worker incarnation, regenerated on process startup."
);
identity_id!(
    ExecutionAttemptId,
    "One claimed job execution, independent of provider retries."
);
identity_id!(
    ConfigurationId,
    "An immutable, host-resolved runtime definition."
);

/// How the host obtained a subject. This is diagnostic data, not a permission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum PrincipalKind {
    Local,
    User,
    Application,
    Workload,
}

/// Immutable call attribution captured by a trusted host.
///
/// Store this beside durable execution metadata and carry it to child calls.
/// Policy revisions are positive; refreshing a policy produces a new scope
/// instead of mutating the authority of an existing permission request.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PrincipalScope {
    tenant_id: TenantId,
    principal_id: PrincipalId,
    kind: PrincipalKind,
    client_id: Option<ClientId>,
    policy_revision: NonZeroU64,
}

impl PrincipalScope {
    #[must_use]
    pub fn new(
        tenant_id: TenantId,
        principal_id: PrincipalId,
        kind: PrincipalKind,
        client_id: Option<ClientId>,
        policy_revision: NonZeroU64,
    ) -> Self {
        Self {
            tenant_id,
            principal_id,
            kind,
            client_id,
            policy_revision,
        }
    }

    /// Attribution for the existing single-user local profile.
    ///
    /// Public HTTP authentication must never synthesize this scope as a fallback.
    #[must_use]
    pub fn local() -> Self {
        Self::new(
            TenantId("local".to_owned()),
            PrincipalId("local-user".to_owned()),
            PrincipalKind::Local,
            None,
            NonZeroU64::MIN,
        )
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
    pub const fn kind(&self) -> PrincipalKind {
        self.kind
    }

    #[must_use]
    pub fn client_id(&self) -> Option<&ClientId> {
        self.client_id.as_ref()
    }

    #[must_use]
    pub const fn policy_revision(&self) -> NonZeroU64 {
        self.policy_revision
    }

    /// Stable ownership without a transient client or authorization revision.
    #[must_use]
    pub fn owner(&self) -> PrincipalKey {
        PrincipalKey {
            tenant_id: self.tenant_id.clone(),
            principal_id: self.principal_id.clone(),
        }
    }
}

/// A private resource's stable owner. This value is not a permission grant.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PrincipalKey {
    pub tenant_id: TenantId,
    pub principal_id: PrincipalId,
}

/// Ownership is independent of the acting client and the current policy revision.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(tag = "type", content = "id", rename_all = "camelCase")]
pub enum ResourceOwner {
    Principal(PrincipalId),
    Group(GroupId),
    Organization,
}

/// Logical namespace for Memory, artifacts, caches and other owned resources.
///
/// Namespace identity is not a filesystem path. An adapter must choose and
/// validate its own path/object representation.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResourceNamespace {
    pub tenant_id: TenantId,
    pub owner: ResourceOwner,
    pub project_id: Option<ProjectId>,
    pub workspace_id: Option<WorkspaceId>,
}

impl ResourceNamespace {
    #[must_use]
    pub fn private(scope: &PrincipalScope) -> Self {
        Self {
            tenant_id: scope.tenant_id.clone(),
            owner: ResourceOwner::Principal(scope.principal_id.clone()),
            project_id: None,
            workspace_id: None,
        }
    }

    /// Tests ownership only; callers must still check the operation's policy.
    ///
    /// Organization and group resources require an explicit ACL decision.
    #[must_use]
    pub fn is_private_owner(&self, scope: &PrincipalScope) -> bool {
        self.tenant_id == scope.tenant_id
            && matches!(&self.owner, ResourceOwner::Principal(id) if id == &scope.principal_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(tenant: &str, subject: &str) -> PrincipalScope {
        PrincipalScope::new(
            TenantId::new(tenant).expect("tenant"),
            PrincipalId::new(subject).expect("subject"),
            PrincipalKind::User,
            Some(ClientId::new("web-client").expect("client")),
            NonZeroU64::new(3).expect("revision"),
        )
    }

    #[test]
    fn identifiers_reject_paths_controls_and_empty_values() {
        for invalid in [
            "", "../alice", "/tenant", "a/b", "a\\b", "a b", "a\nb", "租户",
        ] {
            assert!(TenantId::new(invalid).is_err(), "{invalid:?}");
            assert!(serde_json::from_value::<TenantId>(serde_json::json!(invalid)).is_err());
        }
        assert!(TenantId::new("a".repeat(129)).is_err());
        assert!(TenantId::new("a".repeat(128)).is_ok());
        assert!(TenantId::new("8c85080d-a1a2-4b5c-89dd-00484db23d05").is_ok());
    }

    #[test]
    fn same_subject_in_another_tenant_is_not_the_owner() {
        let alice = scope("organization-a", "alice");
        let namespace = ResourceNamespace::private(&alice);
        assert!(namespace.is_private_owner(&alice));
        assert!(!namespace.is_private_owner(&scope("organization-b", "alice")));
        assert!(!namespace.is_private_owner(&scope("organization-a", "bob")));
        let shared = ResourceNamespace {
            owner: ResourceOwner::Organization,
            ..namespace
        };
        assert!(!shared.is_private_owner(&alice));
    }

    #[test]
    fn scope_round_trip_preserves_client_and_rejects_invalid_revision() {
        let original = scope("organization-a", "alice");
        let mut wire = serde_json::to_value(&original).expect("scope");
        assert_eq!(
            serde_json::from_value::<PrincipalScope>(wire.clone()).expect("decode"),
            original
        );
        wire["policyRevision"] = serde_json::json!(0);
        assert!(serde_json::from_value::<PrincipalScope>(wire).is_err());
        let mut wire = serde_json::to_value(&original).expect("scope");
        wire["authorized"] = serde_json::json!(true);
        assert!(serde_json::from_value::<PrincipalScope>(wire).is_err());
    }

    #[test]
    fn identifier_schema_records_the_runtime_boundary() {
        let schema = serde_json::to_value(schemars::schema_for!(TenantId)).expect("schema");
        assert_eq!(schema["minLength"], 1);
        assert_eq!(schema["maxLength"], 128);
        assert_eq!(schema["pattern"], ID_PATTERN);
    }
}
