//! Request-scoped delegation to one authenticated execution gateway.
//! A ticket is request admission, not tool approval or proof of current lease.

use std::{collections::BTreeMap, fmt, sync::Arc};

use aws_lc_rs::hmac;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zuno_application::{environment::wire::GatewayRequest, runtime::ExecutionLease};
use zuno_types::identity::GatewayId;

use crate::{
    AccessTokenVerifier,
    worker::{
        AuthenticatedWorker, JobGrantToken, VerifiedJobGrant, WorkerAuthError, WorkerAuthority,
        WorkerSubject,
    },
};

const PURPOSE: &str = "zuno.enterprise.gateway-request";

pub struct GatewayServiceAuthority {
    identities: WorkerAuthority,
    gateways: BTreeMap<WorkerSubject, GatewayId>,
}

#[derive(Debug, Clone)]
pub struct AuthenticatedGateway {
    identity: AuthenticatedWorker,
    id: GatewayId,
}

impl AuthenticatedGateway {
    pub fn id(&self) -> &GatewayId {
        &self.id
    }
    pub fn subject(&self) -> &WorkerSubject {
        self.identity.subject()
    }
}

impl GatewayServiceAuthority {
    pub fn new(
        verifier: Arc<dyn AccessTokenVerifier>,
        gateways: BTreeMap<WorkerSubject, GatewayId>,
    ) -> Result<Self, WorkerAuthError> {
        let identities = WorkerAuthority::new(verifier, gateways.keys().cloned().collect())?;
        Ok(Self {
            identities,
            gateways,
        })
    }

    pub async fn authenticate(&self, token: &str) -> Result<AuthenticatedGateway, WorkerAuthError> {
        let identity = self.identities.authenticate(token).await?;
        let id = self
            .gateways
            .get(identity.subject())
            .ok_or(WorkerAuthError::Denied)?
            .clone();
        Ok(AuthenticatedGateway { identity, id })
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct GatewayTicket(JobGrantToken);
impl GatewayTicket {
    pub fn expose(&self) -> &str {
        self.0.expose()
    }
}
impl fmt::Debug for GatewayTicket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("GatewayTicket([redacted])")
    }
}
impl TryFrom<String> for GatewayTicket {
    type Error = WorkerAuthError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Ok(Self(JobGrantToken::try_from(value)?))
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Claims {
    version: u32,
    purpose: String,
    gateway: GatewayId,
    worker: WorkerSubject,
    lease: ExecutionLease,
    request_sha256: String,
    expires_at_ms: i64,
}

/// Sealed only after gateway service authentication and ticket verification.
pub struct VerifiedGatewayRequest {
    worker: WorkerSubject,
    lease: ExecutionLease,
    expires_at_ms: i64,
}
impl VerifiedGatewayRequest {
    pub fn worker(&self) -> &WorkerSubject {
        &self.worker
    }
    pub fn lease(&self) -> &ExecutionLease {
        &self.lease
    }
    pub fn expires_at_ms(&self) -> i64 {
        self.expires_at_ms
    }
}

/// Kept only by the control plane. Gateways redeem tickets through authenticated
/// state calls; neither Workers nor gateways receive this signing keyring.
pub struct GatewayTicketAuthority {
    active: String,
    keys: BTreeMap<String, hmac::Key>,
    lifetime_ms: i64,
}
impl GatewayTicketAuthority {
    pub fn new(
        active: String,
        keys: Vec<(String, Vec<u8>)>,
        lifetime_ms: i64,
    ) -> Result<Self, WorkerAuthError> {
        if keys.is_empty() || keys.len() > 8 || !(1000..=30000).contains(&lifetime_ms) {
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
            lifetime_ms,
        })
    }

    /// The host verifies the Worker grant and current data-owner lease first.
    pub fn issue(
        &self,
        grant: &VerifiedJobGrant,
        gateway: GatewayId,
        request: &GatewayRequest,
        now_ms: i64,
    ) -> Result<GatewayTicket, WorkerAuthError> {
        if now_ms < 0 || now_ms >= grant.expires_at_ms() {
            return Err(WorkerAuthError::Expired);
        }
        let claims = Claims {
            version: 1,
            purpose: PURPOSE.to_owned(),
            gateway,
            worker: grant.subject().clone(),
            lease: grant.lease().clone(),
            request_sha256: request_digest(request)?,
            expires_at_ms: grant
                .expires_at_ms()
                .min(now_ms.saturating_add(self.lifetime_ms)),
        };
        let encoded = URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&claims).map_err(|_| WorkerAuthError::InvalidGrant)?);
        let signed = format!("{}.{}", self.active, encoded);
        let tag = hmac::sign(
            self.keys.get(&self.active).expect("validated key"),
            signed.as_bytes(),
        );
        GatewayTicket::try_from(format!("{signed}.{}", URL_SAFE_NO_PAD.encode(tag.as_ref())))
    }

    /// Verification never grants execution. The data owner still checks current
    /// membership, environment assignment, lease and operation approval.
    pub fn verify(
        &self,
        gateway: &AuthenticatedGateway,
        ticket: &GatewayTicket,
        request: &GatewayRequest,
        now_ms: i64,
    ) -> Result<VerifiedGatewayRequest, WorkerAuthError> {
        let mut parts = ticket.expose().split('.');
        let key_id = parts.next().ok_or(WorkerAuthError::InvalidGrant)?;
        let payload = parts.next().ok_or(WorkerAuthError::InvalidGrant)?;
        let signature = parts.next().ok_or(WorkerAuthError::InvalidGrant)?;
        let key = self.keys.get(key_id).ok_or(WorkerAuthError::InvalidGrant)?;
        let tag = URL_SAFE_NO_PAD
            .decode(signature)
            .map_err(|_| WorkerAuthError::InvalidGrant)?;
        hmac::verify(key, format!("{key_id}.{payload}").as_bytes(), &tag)
            .map_err(|_| WorkerAuthError::InvalidGrant)?;
        let claims: Claims = serde_json::from_slice(
            &URL_SAFE_NO_PAD
                .decode(payload)
                .map_err(|_| WorkerAuthError::InvalidGrant)?,
        )
        .map_err(|_| WorkerAuthError::InvalidGrant)?;
        if claims.version != 1
            || claims.purpose != PURPOSE
            || claims.gateway != *gateway.id()
            || claims.worker.tenant_id != gateway.subject().tenant_id
            || claims.lease.owner.tenant_id != gateway.subject().tenant_id
            || claims.lease.epoch == 0
            || claims.request_sha256 != request_digest(request)?
            || claims.expires_at_ms > claims.lease.expires_at_ms
        {
            return Err(WorkerAuthError::Denied);
        }
        if now_ms < 0
            || now_ms >= claims.expires_at_ms
            || now_ms >= gateway.identity.expires_at_ms()
        {
            return Err(WorkerAuthError::Expired);
        }
        Ok(VerifiedGatewayRequest {
            worker: claims.worker,
            lease: claims.lease,
            expires_at_ms: claims.expires_at_ms,
        })
    }
}

fn request_digest(request: &GatewayRequest) -> Result<String, WorkerAuthError> {
    let bytes = request
        .encode()
        .map_err(|_| WorkerAuthError::InvalidGrant)?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{IdentityError, VerifiedIdentity, VerifiedIdentityKind, worker::JobGrantAuthority};
    use zuno_application::environment::wire::GatewayCommand;
    use zuno_types::identity::*;

    struct Verifier;
    #[async_trait::async_trait]
    impl AccessTokenVerifier for Verifier {
        async fn verify(&self, token: &str) -> Result<VerifiedIdentity, IdentityError> {
            let kind = if token == "user" {
                VerifiedIdentityKind::DelegatedUser
            } else {
                VerifiedIdentityKind::Workload
            };
            Ok(VerifiedIdentity::from_verified(
                "https://issuer.example".to_owned(),
                TenantId::new("tenant").unwrap(),
                PrincipalId::new(token).unwrap(),
                ClientId::new("service-app").unwrap(),
                "service-app".to_owned(),
                kind,
                100,
            ))
        }
    }
    fn subject(id: &str) -> WorkerSubject {
        WorkerSubject {
            tenant_id: TenantId::new("tenant").unwrap(),
            principal_id: PrincipalId::new(id).unwrap(),
            client_id: ClientId::new("service-app").unwrap(),
        }
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
            worker: WorkerInstanceId::new("worker-instance").unwrap(),
            epoch: 1,
            checkpoint_version: 0,
            expires_at_ms: 20000,
        }
    }
    fn tickets(key: &str, entries: Vec<(String, Vec<u8>)>) -> GatewayTicketAuthority {
        GatewayTicketAuthority::new(key.to_owned(), entries, 5000).unwrap()
    }

    #[tokio::test]
    async fn gateway_delegation_binds_service_request_and_expiry_without_widening_worker_grants() {
        let workers = WorkerAuthority::new(Arc::new(Verifier), [subject("worker")].into()).unwrap();
        let worker = workers.authenticate("worker").await.unwrap();
        let grants = JobGrantAuthority::new(
            "current".to_owned(),
            vec![("current".to_owned(), vec![1; 32])],
        )
        .unwrap();
        let grant_token = grants.issue(&worker, &lease(), 1000).unwrap();
        let grant = grants.verify(&worker, &grant_token, 1000).unwrap();
        let gateways = GatewayServiceAuthority::new(
            Arc::new(Verifier),
            [
                (subject("gateway-a"), GatewayId::new("a").unwrap()),
                (subject("gateway-b"), GatewayId::new("b").unwrap()),
                (
                    subject("user"),
                    GatewayId::new("user-cannot-be-a-gateway").unwrap(),
                ),
            ]
            .into(),
        )
        .unwrap();
        assert!(gateways.authenticate("user").await.is_err());
        assert!(gateways.authenticate("worker").await.is_err());
        let a = gateways.authenticate("gateway-a").await.unwrap();
        let b = gateways.authenticate("gateway-b").await.unwrap();
        let authority = tickets("current", vec![("current".to_owned(), vec![2; 32])]);
        let request = GatewayRequest::new(GatewayCommand::Acquire).unwrap();
        let ticket = authority
            .issue(&grant, GatewayId::new("a").unwrap(), &request, 1000)
            .unwrap();
        let verified = authority.verify(&a, &ticket, &request, 2000).unwrap();
        assert_eq!(verified.worker(), worker.subject());
        assert_eq!(verified.lease(), &lease());
        assert_eq!(verified.expires_at_ms(), 6000);
        assert!(authority.verify(&a, &ticket, &request, 6000).is_err());
        assert!(authority.verify(&b, &ticket, &request, 2000).is_err());
        assert!(
            authority
                .verify(
                    &a,
                    &ticket,
                    &GatewayRequest::new(GatewayCommand::Get).unwrap(),
                    2000
                )
                .is_err()
        );
        let wrong_purpose = GatewayTicket::try_from(grant_token.expose().to_owned()).unwrap();
        let shared_key = tickets("current", vec![("current".to_owned(), vec![1; 32])]);
        assert!(
            shared_key
                .verify(&a, &wrong_purpose, &request, 2000)
                .is_err()
        );
        let as_worker = JobGrantToken::try_from(ticket.expose().to_owned()).unwrap();
        assert!(grants.verify(&worker, &as_worker, 2000).is_err());
        let rotated = tickets(
            "next",
            vec![
                ("current".to_owned(), vec![2; 32]),
                ("next".to_owned(), vec![3; 32]),
            ],
        );
        rotated.verify(&a, &ticket, &request, 2000).unwrap();
        let retired = tickets("next", vec![("next".to_owned(), vec![3; 32])]);
        assert!(retired.verify(&a, &ticket, &request, 2000).is_err());
        let mut fields = ticket
            .expose()
            .split('.')
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let mut payload: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(&fields[1]).unwrap()).unwrap();
        payload["lease"]["epoch"] = serde_json::json!(2);
        fields[1] = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap());
        let changed = GatewayTicket::try_from(fields.join(".")).unwrap();
        assert!(authority.verify(&a, &changed, &request, 2000).is_err());
        assert!(!format!("{ticket:?}").contains(ticket.expose()));
    }
}
