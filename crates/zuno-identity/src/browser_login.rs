//! BFF authentication service. Storage and transport are host-installed ports.

use std::sync::Arc;

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use sha2::{Digest, Sha256};
use zuno_auth::Secret;

use crate::login::{LoginError, OidcLoginClient};
use crate::login_state::{
    BrowserSessionRecord, BrowserSessionStore, LoginStateCipher, LoginTransactionStore,
};
use crate::{VerifiedIdentity, VerifiedIdentityKind};

pub struct BrowserLoginService {
    client: Arc<OidcLoginClient>,
    cipher: Arc<LoginStateCipher>,
    transactions: Arc<dyn LoginTransactionStore>,
    sessions: Arc<dyn BrowserSessionStore>,
}

pub struct BrowserLoginStart {
    pub authorization_url: reqwest::Url,
    pub browser_binding: Secret,
    pub expires_at_seconds: u64,
}

pub struct BrowserLoginComplete {
    pub session_token: Secret,
    pub identity: VerifiedIdentity,
    pub expires_at_seconds: u64,
}

impl BrowserLoginService {
    pub fn redirect_uri(&self) -> &reqwest::Url {
        self.client.redirect_uri()
    }

    pub fn issuer(&self) -> &str {
        self.client.issuer()
    }

    pub fn new(
        client: Arc<OidcLoginClient>,
        cipher: Arc<LoginStateCipher>,
        transactions: Arc<dyn LoginTransactionStore>,
        sessions: Arc<dyn BrowserSessionStore>,
    ) -> Self {
        Self {
            client,
            cipher,
            transactions,
            sessions,
        }
    }

    pub async fn begin(&self) -> Result<BrowserLoginStart, LoginError> {
        let binding = random_token()?;
        let start = self.client.begin(binding.expose())?;
        let expires_at_seconds = start.attempt.expires_at_seconds();
        let transaction = self.cipher.seal(start.attempt)?;
        self.transactions.insert(transaction).await?;
        Ok(BrowserLoginStart {
            authorization_url: start.authorization_url,
            browser_binding: binding,
            expires_at_seconds,
        })
    }

    pub async fn complete(
        &self,
        state: &str,
        binding: &str,
        code: &str,
    ) -> Result<BrowserLoginComplete, LoginError> {
        validate_random_token(state)?;
        validate_random_token(binding)?;
        let now = jsonwebtoken::get_current_timestamp();
        let transaction = self
            .transactions
            .take(token_hash(state), token_hash(binding), now)
            .await?
            .ok_or(LoginError::Transaction)?;
        let attempt = self.cipher.open(transaction, state, binding, now)?;
        let login = self.client.complete(attempt, state, binding, code).await?;
        let token = random_token()?;
        let session = BrowserSessionRecord::from_identity(
            token_hash(token.expose()),
            &login.identity,
            login.expires_at_seconds,
        )?;
        self.sessions.create(session).await?;
        Ok(BrowserLoginComplete {
            session_token: token,
            identity: login.identity,
            expires_at_seconds: login.expires_at_seconds,
        })
    }

    /// A backend outage is an error, not proof that a valid cookie was revoked.
    pub async fn authenticate(&self, cookie: &str) -> Result<Option<VerifiedIdentity>, LoginError> {
        if validate_random_token(cookie).is_err() {
            return Ok(None);
        }
        let now = jsonwebtoken::get_current_timestamp();
        let Some(record) = self.sessions.lookup(token_hash(cookie), now).await? else {
            return Ok(None);
        };
        if record.token_hash != token_hash(cookie)
            || record.expires_at <= now
            || record.issuer != self.client.issuer()
            || record.oauth_client_id != self.client.client_id()
        {
            return Err(LoginError::Transaction);
        }
        Ok(Some(VerifiedIdentity::from_verified(
            record.issuer,
            record.tenant_id,
            record.principal_id,
            record.client_id,
            record.oauth_client_id,
            VerifiedIdentityKind::DelegatedUser,
            record.expires_at,
        )))
    }

    pub async fn logout(&self, cookie: &str) -> Result<(), LoginError> {
        if validate_random_token(cookie).is_err() {
            return Ok(());
        }
        self.sessions.revoke(token_hash(cookie)).await
    }

    /// An authenticated provider error consumes only this browser's attempt.
    pub async fn cancel(&self, state: &str, binding: &str) -> Result<(), LoginError> {
        validate_random_token(state)?;
        validate_random_token(binding)?;
        let now = jsonwebtoken::get_current_timestamp();
        let transaction = self
            .transactions
            .take(token_hash(state), token_hash(binding), now)
            .await?
            .ok_or(LoginError::Transaction)?;
        self.cipher.open(transaction, state, binding, now)?;
        Ok(())
    }
}

pub(crate) fn token_hash(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

fn random_token() -> Result<Secret, LoginError> {
    use aws_lc_rs::rand::SecureRandom as _;
    let mut bytes = [0u8; 32];
    aws_lc_rs::rand::SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| LoginError::Transaction)?;
    Ok(Secret::new(URL_SAFE_NO_PAD.encode(bytes)))
}

fn validate_random_token(token: &str) -> Result<(), LoginError> {
    if token.len() != 43
        || URL_SAFE_NO_PAD
            .decode(token)
            .map_or(true, |bytes| bytes.len() != 32)
        || !token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte))
    {
        return Err(LoginError::Transaction);
    }
    Ok(())
}
