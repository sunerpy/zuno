use super::*;
use zuno_application::learning::LearningExecutionLease;

const LEARNING_PURPOSE: &str = "zuno.enterprise.learning-job";

#[derive(Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LearningGrantToken(JobGrantToken);
impl LearningGrantToken {
    pub fn expose(&self) -> &str {
        self.0.expose()
    }
}
impl std::fmt::Debug for LearningGrantToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LearningGrantToken([redacted])")
    }
}
impl TryFrom<String> for LearningGrantToken {
    type Error = WorkerAuthError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Ok(Self(JobGrantToken::try_from(value)?))
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LearningClaims {
    version: u32,
    purpose: String,
    subject: WorkerSubject,
    lease: LearningExecutionLease,
    expires_at_ms: i64,
}

pub struct VerifiedLearningGrant {
    lease: LearningExecutionLease,
    expires_at_ms: i64,
}
impl VerifiedLearningGrant {
    pub fn lease(&self) -> &LearningExecutionLease {
        &self.lease
    }
    pub fn expires_at_ms(&self) -> i64 {
        self.expires_at_ms
    }
}

impl JobGrantAuthority {
    pub fn issue_learning(
        &self,
        worker: &AuthenticatedWorker,
        lease: &LearningExecutionLease,
        now_ms: i64,
    ) -> Result<LearningGrantToken, WorkerAuthError> {
        lease
            .validate()
            .map_err(|_| WorkerAuthError::InvalidGrant)?;
        if worker.subject.tenant_id != lease.owner.tenant_id || now_ms < 0 {
            return Err(WorkerAuthError::Denied);
        }
        let expires_at_ms = lease.expires_at_ms.min(worker.expires_at_ms);
        if expires_at_ms <= now_ms || expires_at_ms.saturating_sub(now_ms) > 300_000 {
            return Err(WorkerAuthError::Expired);
        }
        let claims = LearningClaims {
            version: 1,
            purpose: LEARNING_PURPOSE.to_owned(),
            subject: worker.subject.clone(),
            lease: lease.clone(),
            expires_at_ms,
        };
        let payload = URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&claims).map_err(|_| WorkerAuthError::InvalidGrant)?);
        let signed = format!("{}.{}", self.active, payload);
        let tag = hmac::sign(
            self.keys.get(&self.active).expect("validated active key"),
            signed.as_bytes(),
        );
        LearningGrantToken::try_from(format!("{signed}.{}", URL_SAFE_NO_PAD.encode(tag.as_ref())))
    }

    pub fn verify_learning(
        &self,
        worker: &AuthenticatedWorker,
        token: &LearningGrantToken,
        now_ms: i64,
    ) -> Result<VerifiedLearningGrant, WorkerAuthError> {
        self.verify_learning_inner(worker, token, now_ms, true)
    }

    /// A late model receipt can be checked against its prior admission. This
    /// verification grants no new request, renewal or Memory commit.
    pub fn verify_learning_receipt(
        &self,
        worker: &AuthenticatedWorker,
        token: &LearningGrantToken,
        now_ms: i64,
    ) -> Result<VerifiedLearningGrant, WorkerAuthError> {
        self.verify_learning_inner(worker, token, now_ms, false)
    }

    fn verify_learning_inner(
        &self,
        worker: &AuthenticatedWorker,
        token: &LearningGrantToken,
        now_ms: i64,
        require_current: bool,
    ) -> Result<VerifiedLearningGrant, WorkerAuthError> {
        let parts = token.expose().split('.').collect::<Vec<_>>();
        let [key_id, payload, signature] = parts.as_slice() else {
            return Err(WorkerAuthError::InvalidGrant);
        };
        let key = self
            .keys
            .get(*key_id)
            .ok_or(WorkerAuthError::InvalidGrant)?;
        let signature = URL_SAFE_NO_PAD
            .decode(signature)
            .map_err(|_| WorkerAuthError::InvalidGrant)?;
        hmac::verify(key, format!("{key_id}.{payload}").as_bytes(), &signature)
            .map_err(|_| WorkerAuthError::InvalidGrant)?;
        let claims: LearningClaims = serde_json::from_slice(
            &URL_SAFE_NO_PAD
                .decode(payload)
                .map_err(|_| WorkerAuthError::InvalidGrant)?,
        )
        .map_err(|_| WorkerAuthError::InvalidGrant)?;
        claims
            .lease
            .validate()
            .map_err(|_| WorkerAuthError::InvalidGrant)?;
        if claims.version != 1
            || claims.purpose != LEARNING_PURPOSE
            || claims.subject != worker.subject
            || claims.lease.owner.tenant_id != worker.subject.tenant_id
            || claims.expires_at_ms > claims.lease.expires_at_ms
        {
            return Err(WorkerAuthError::Denied);
        }
        if now_ms < 0
            || require_current && now_ms >= claims.expires_at_ms
            || now_ms >= worker.expires_at_ms
        {
            return Err(WorkerAuthError::Expired);
        }
        Ok(VerifiedLearningGrant {
            lease: claims.lease,
            expires_at_ms: claims.expires_at_ms,
        })
    }
}
