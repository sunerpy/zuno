//! Workload authentication and short-lived, lease-bound Worker grants.

use crate::{AccessTokenVerifier, IdentityError, VerifiedIdentity, VerifiedIdentityKind};
use aws_lc_rs::hmac;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;
use zuno_application::runtime::ExecutionLease;
use zuno_types::identity::{ClientId, PrincipalId, TenantId};

const GRANT_PURPOSE: &str = "zuno.enterprise.worker-state";
const MAX_GRANT_BYTES: usize = 8192;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkerSubject {
    pub tenant_id: TenantId,
    pub principal_id: PrincipalId,
    pub client_id: ClientId,
}

/// Constructible only through a verified workload access token and allowlist.
#[derive(Debug, Clone)]
pub struct AuthenticatedWorker {
    subject: WorkerSubject,
    expires_at_ms: i64,
}
impl AuthenticatedWorker {
    pub fn subject(&self) -> &WorkerSubject {
        &self.subject
    }
    pub const fn expires_at_ms(&self) -> i64 {
        self.expires_at_ms
    }
}

pub struct WorkerAuthority {
    verifier: Arc<dyn AccessTokenVerifier>,
    allowed: BTreeSet<WorkerSubject>,
}
impl WorkerAuthority {
    pub fn new(
        verifier: Arc<dyn AccessTokenVerifier>,
        allowed: BTreeSet<WorkerSubject>,
    ) -> Result<Self, WorkerAuthError> {
        if allowed.is_empty() || allowed.len() > 256 {
            return Err(WorkerAuthError::Configuration);
        }
        Ok(Self { verifier, allowed })
    }

    pub async fn authenticate(
        &self,
        access_token: &str,
    ) -> Result<AuthenticatedWorker, WorkerAuthError> {
        let identity = self.verifier.verify(access_token).await?;
        self.accept(identity)
    }

    fn accept(&self, identity: VerifiedIdentity) -> Result<AuthenticatedWorker, WorkerAuthError> {
        if identity.kind() != VerifiedIdentityKind::Workload {
            return Err(WorkerAuthError::Denied);
        }
        let subject = WorkerSubject {
            tenant_id: identity.tenant_id().clone(),
            principal_id: identity.principal_id().clone(),
            client_id: identity.client_id().clone(),
        };
        if !self.allowed.contains(&subject) {
            return Err(WorkerAuthError::Denied);
        }
        let expires_at_ms = identity
            .expires_at_seconds()
            .checked_mul(1000)
            .and_then(|value| i64::try_from(value).ok())
            .ok_or(WorkerAuthError::Denied)?;
        Ok(AuthenticatedWorker {
            subject,
            expires_at_ms,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WorkerAuthError {
    #[error(transparent)]
    Identity(#[from] IdentityError),
    #[error("workload is not authorized for Worker state access")]
    Denied,
    #[error("invalid Worker execution grant")]
    InvalidGrant,
    #[error("Worker execution grant expired")]
    Expired,
    #[error("invalid Worker authority configuration")]
    Configuration,
}

/// A bearer secret for an internal authenticated channel, never a Web DTO.
#[derive(Clone, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct JobGrantToken(String);
impl JobGrantToken {
    pub fn expose(&self) -> &str {
        &self.0
    }
}
impl fmt::Debug for JobGrantToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("JobGrantToken([redacted])")
    }
}
impl TryFrom<String> for JobGrantToken {
    type Error = WorkerAuthError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.is_empty()
            || value.len() > MAX_GRANT_BYTES
            || !value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
            || value.split('.').count() != 3
        {
            return Err(WorkerAuthError::InvalidGrant);
        }
        Ok(Self(value))
    }
}
impl From<JobGrantToken> for String {
    fn from(token: JobGrantToken) -> Self {
        token.0
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct GrantClaims {
    version: u32,
    purpose: String,
    subject: WorkerSubject,
    lease: ExecutionLease,
    expires_at_ms: i64,
}

/// The signature grants access only to the named lease. The state provider must
/// still check current database time, epoch, policy, ownership and checkpoint.
#[derive(Debug, Clone)]
pub struct VerifiedJobGrant {
    lease: ExecutionLease,
    expires_at_ms: i64,
}
impl VerifiedJobGrant {
    pub fn lease(&self) -> &ExecutionLease {
        &self.lease
    }
    pub const fn expires_at_ms(&self) -> i64 {
        self.expires_at_ms
    }
}

pub struct JobGrantAuthority {
    active: String,
    keys: BTreeMap<String, hmac::Key>,
}
impl JobGrantAuthority {
    pub fn new(active: String, keys: Vec<(String, Vec<u8>)>) -> Result<Self, WorkerAuthError> {
        if keys.is_empty() || keys.len() > 8 {
            return Err(WorkerAuthError::Configuration);
        }
        let mut accepted = BTreeMap::new();
        for (id, key) in keys {
            if id.is_empty()
                || id.len() > 32
                || !id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
                || !(32..=64).contains(&key.len())
                || accepted
                    .insert(id, hmac::Key::new(hmac::HMAC_SHA256, &key))
                    .is_some()
            {
                return Err(WorkerAuthError::Configuration);
            }
        }
        if !accepted.contains_key(&active) {
            return Err(WorkerAuthError::Configuration);
        }
        Ok(Self {
            active,
            keys: accepted,
        })
    }

    pub fn issue(
        &self,
        worker: &AuthenticatedWorker,
        lease: &ExecutionLease,
        now_ms: i64,
    ) -> Result<JobGrantToken, WorkerAuthError> {
        if worker.subject.tenant_id != lease.owner.tenant_id || lease.epoch == 0 || now_ms < 0 {
            return Err(WorkerAuthError::Denied);
        }
        let expires_at_ms = lease.expires_at_ms.min(worker.expires_at_ms);
        if expires_at_ms <= now_ms || expires_at_ms.saturating_sub(now_ms) > 300_000 {
            return Err(WorkerAuthError::Expired);
        }
        let payload = serde_json::to_vec(&GrantClaims {
            version: 1,
            purpose: GRANT_PURPOSE.to_owned(),
            subject: worker.subject.clone(),
            lease: lease.clone(),
            expires_at_ms,
        })
        .map_err(|_| WorkerAuthError::InvalidGrant)?;
        let payload = URL_SAFE_NO_PAD.encode(payload);
        let signed = format!("{}.{}", self.active, payload);
        let tag = hmac::sign(
            self.keys.get(&self.active).expect("validated active key"),
            signed.as_bytes(),
        );
        JobGrantToken::try_from(format!("{signed}.{}", URL_SAFE_NO_PAD.encode(tag.as_ref())))
    }

    pub fn verify(
        &self,
        worker: &AuthenticatedWorker,
        token: &JobGrantToken,
        now_ms: i64,
    ) -> Result<VerifiedJobGrant, WorkerAuthError> {
        let mut fields = token.expose().split('.');
        let key_id = fields.next().ok_or(WorkerAuthError::InvalidGrant)?;
        let payload = fields.next().ok_or(WorkerAuthError::InvalidGrant)?;
        let signature = fields.next().ok_or(WorkerAuthError::InvalidGrant)?;
        let key = self.keys.get(key_id).ok_or(WorkerAuthError::InvalidGrant)?;
        let signature = URL_SAFE_NO_PAD
            .decode(signature)
            .map_err(|_| WorkerAuthError::InvalidGrant)?;
        let signed = format!("{key_id}.{payload}");
        hmac::verify(key, signed.as_bytes(), &signature)
            .map_err(|_| WorkerAuthError::InvalidGrant)?;
        let claims: GrantClaims = serde_json::from_slice(
            &URL_SAFE_NO_PAD
                .decode(payload)
                .map_err(|_| WorkerAuthError::InvalidGrant)?,
        )
        .map_err(|_| WorkerAuthError::InvalidGrant)?;
        if claims.version != 1
            || claims.purpose != GRANT_PURPOSE
            || claims.subject != worker.subject
            || claims.lease.owner.tenant_id != worker.subject.tenant_id
            || claims.lease.epoch == 0
            || claims.expires_at_ms > claims.lease.expires_at_ms
        {
            return Err(WorkerAuthError::Denied);
        }
        if now_ms < 0 || now_ms >= claims.expires_at_ms || now_ms >= worker.expires_at_ms {
            return Err(WorkerAuthError::Expired);
        }
        Ok(VerifiedJobGrant {
            lease: claims.lease,
            expires_at_ms: claims.expires_at_ms,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zuno_types::identity::{
        ExecutionAttemptId, JobId, PrincipalKey, SessionId, WorkerInstanceId,
    };

    struct RejectVerifier;
    #[async_trait::async_trait]
    impl AccessTokenVerifier for RejectVerifier {
        async fn verify(&self, _token: &str) -> Result<VerifiedIdentity, IdentityError> {
            Err(crate::TokenRejection::InvalidToken.into())
        }
    }
    fn subject(client: &str) -> WorkerSubject {
        WorkerSubject {
            tenant_id: TenantId::new("tenant").unwrap(),
            principal_id: PrincipalId::new("worker-service").unwrap(),
            client_id: ClientId::new(client).unwrap(),
        }
    }
    fn authority() -> WorkerAuthority {
        WorkerAuthority::new(
            Arc::new(RejectVerifier),
            [subject("client"), subject("other-client")].into(),
        )
        .unwrap()
    }
    fn identity(kind: VerifiedIdentityKind, client: &str) -> VerifiedIdentity {
        let subject = subject(client);
        VerifiedIdentity::from_verified(
            subject.tenant_id,
            subject.principal_id,
            subject.client_id,
            kind,
            10,
        )
    }
    fn worker(client: &str) -> AuthenticatedWorker {
        authority()
            .accept(identity(VerifiedIdentityKind::Workload, client))
            .unwrap()
    }
    fn lease() -> ExecutionLease {
        ExecutionLease {
            owner: PrincipalKey {
                tenant_id: TenantId::new("tenant").unwrap(),
                principal_id: PrincipalId::new("alice").unwrap(),
            },
            job_id: JobId::new("job").unwrap(),
            session_id: SessionId::new("session").unwrap(),
            attempt_id: ExecutionAttemptId::new("attempt").unwrap(),
            worker: WorkerInstanceId::new("incarnation").unwrap(),
            epoch: 9,
            checkpoint_version: 4,
            expires_at_ms: 5000,
        }
    }
    fn grants() -> JobGrantAuthority {
        JobGrantAuthority::new(
            "current".to_owned(),
            vec![("current".to_owned(), vec![1; 32])],
        )
        .unwrap()
    }

    #[test]
    fn user_and_unlisted_application_cannot_become_a_worker() {
        assert!(
            authority()
                .accept(identity(VerifiedIdentityKind::DelegatedUser, "client"))
                .is_err()
        );
        assert!(
            authority()
                .accept(identity(VerifiedIdentityKind::Workload, "not-listed"))
                .is_err()
        );
        assert!(
            authority()
                .accept(identity(VerifiedIdentityKind::Workload, "client"))
                .is_ok()
        );
    }

    #[test]
    fn grant_binds_every_execution_coordinate_and_the_service_application() {
        let authority = grants();
        let worker = worker("client");
        let lease = lease();
        let token = authority.issue(&worker, &lease, 1000).unwrap();
        assert_eq!(
            authority.verify(&worker, &token, 2000).unwrap().lease(),
            &lease
        );
        assert!(!format!("{token:?}").contains(token.expose()));
        assert!(matches!(
            authority.verify(&worker, &token, 5000),
            Err(WorkerAuthError::Expired)
        ));
        assert!(
            authority
                .verify(&super::tests::worker("other-client"), &token, 2000)
                .is_err()
        );
        let mut pieces = token
            .expose()
            .split('.')
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let mut claims: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(&pieces[1]).unwrap()).unwrap();
        claims["lease"]["epoch"] = serde_json::json!(10);
        pieces[1] = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap());
        let changed = JobGrantToken::try_from(pieces.join(".")).unwrap();
        assert!(authority.verify(&worker, &changed, 2000).is_err());
    }

    #[test]
    fn key_rotation_accepts_explicitly_retained_keys_and_rejects_retired_keys() {
        let worker = worker("client");
        let token = grants().issue(&worker, &lease(), 1000).unwrap();
        let rotating = JobGrantAuthority::new(
            "next".to_owned(),
            vec![
                ("current".to_owned(), vec![1; 32]),
                ("next".to_owned(), vec![2; 32]),
            ],
        )
        .unwrap();
        rotating.verify(&worker, &token, 2000).unwrap();
        let retired =
            JobGrantAuthority::new("next".to_owned(), vec![("next".to_owned(), vec![2; 32])])
                .unwrap();
        assert!(retired.verify(&worker, &token, 2000).is_err());
    }

    #[test]
    fn grant_lifetime_cannot_exceed_the_workload_or_cross_a_tenant() {
        let worker = worker("client");
        let mut lease = lease();
        lease.expires_at_ms = 20_000;
        let token = grants().issue(&worker, &lease, 1000).unwrap();
        assert_eq!(
            grants()
                .verify(&worker, &token, 2000)
                .unwrap()
                .expires_at_ms(),
            10_000
        );
        lease.owner.tenant_id = TenantId::new("other").unwrap();
        assert!(grants().issue(&worker, &lease, 1000).is_err());
        assert!(JobGrantToken::try_from("secret with whitespace".to_owned()).is_err());
    }
}
