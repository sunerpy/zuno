use super::*;
use zuno_application::workspace_transfer::{SnapshotTransferAssignment, SnapshotTransferRequest};

const SNAPSHOT_PURPOSE: &str = "zuno.enterprise.snapshot-transfer";

/// This purpose cannot execute commands, upload a project, or read arbitrary
/// files. Source redemption also checks the current lease and organization.
#[derive(Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct GatewaySnapshotTicket(JobGrantToken);
impl GatewaySnapshotTicket {
    pub fn expose(&self) -> &str {
        self.0.expose()
    }
}
impl fmt::Debug for GatewaySnapshotTicket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("GatewaySnapshotTicket([redacted])")
    }
}
impl TryFrom<String> for GatewaySnapshotTicket {
    type Error = WorkerAuthError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Ok(Self(JobGrantToken::try_from(value)?))
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SnapshotClaims {
    version: u32,
    purpose: String,
    source_gateway: GatewayId,
    tenant: zuno_types::identity::TenantId,
    request_sha256: String,
    assignment_sha256: String,
    expires_at_ms: i64,
}

fn request_digest(request: &SnapshotTransferRequest) -> Result<String, WorkerAuthError> {
    Ok(hex::encode(Sha256::digest(
        serde_json::to_vec(request).map_err(|_| WorkerAuthError::InvalidGrant)?,
    )))
}

impl GatewayTicketAuthority {
    pub fn issue_snapshot(
        &self,
        assigned: &SnapshotTransferAssignment,
        now_ms: i64,
    ) -> Result<GatewaySnapshotTicket, WorkerAuthError> {
        if now_ms < 0 || assigned.request.lease.expires_at_ms <= now_ms {
            return Err(WorkerAuthError::Expired);
        }
        let claims = SnapshotClaims {
            version: 1,
            purpose: SNAPSHOT_PURPOSE.to_owned(),
            source_gateway: assigned.source.gateway_id.clone(),
            tenant: assigned.request.lease.owner.tenant_id.clone(),
            request_sha256: request_digest(&assigned.request)?,
            assignment_sha256: assigned.digest(),
            expires_at_ms: assigned
                .request
                .lease
                .expires_at_ms
                .min(now_ms.saturating_add(self.lifetime_ms)),
        };
        let payload = URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&claims).map_err(|_| WorkerAuthError::InvalidGrant)?);
        let signed = format!("{}.{}", self.active, payload);
        let tag = hmac::sign(
            self.keys.get(&self.active).expect("validated key"),
            signed.as_bytes(),
        );
        GatewaySnapshotTicket::try_from(format!(
            "{signed}.{}",
            URL_SAFE_NO_PAD.encode(tag.as_ref())
        ))
    }

    pub fn verify_snapshot(
        &self,
        gateway: &AuthenticatedGateway,
        ticket: &GatewaySnapshotTicket,
        request: &SnapshotTransferRequest,
        now_ms: i64,
    ) -> Result<String, WorkerAuthError> {
        let parts = ticket.expose().split('.').collect::<Vec<_>>();
        let [key_id, payload, signature] = parts.as_slice() else {
            return Err(WorkerAuthError::InvalidGrant);
        };
        let key = self
            .keys
            .get(*key_id)
            .ok_or(WorkerAuthError::InvalidGrant)?;
        let tag = URL_SAFE_NO_PAD
            .decode(signature)
            .map_err(|_| WorkerAuthError::InvalidGrant)?;
        hmac::verify(key, format!("{key_id}.{payload}").as_bytes(), &tag)
            .map_err(|_| WorkerAuthError::InvalidGrant)?;
        let claims: SnapshotClaims = serde_json::from_slice(
            &URL_SAFE_NO_PAD
                .decode(payload)
                .map_err(|_| WorkerAuthError::InvalidGrant)?,
        )
        .map_err(|_| WorkerAuthError::InvalidGrant)?;
        if claims.version != 1
            || claims.purpose != SNAPSHOT_PURPOSE
            || claims.source_gateway != *gateway.id()
            || claims.tenant != gateway.subject().tenant_id
            || claims.tenant != request.lease.owner.tenant_id
            || claims.request_sha256 != request_digest(request)?
        {
            return Err(WorkerAuthError::Denied);
        }
        if now_ms < 0
            || now_ms >= claims.expires_at_ms
            || now_ms >= gateway.identity.expires_at_ms()
        {
            return Err(WorkerAuthError::Expired);
        }
        Ok(claims.assignment_sha256)
    }
}
