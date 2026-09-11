//! Server-only encrypted login transactions and opaque browser sessions.
//! Public handlers never accept these storage records as authentication.

use std::collections::BTreeMap;

use async_trait::async_trait;
use aws_lc_rs::aead::{AES_256_GCM, Aad, Nonce, RandomizedNonceKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zuno_auth::Secret;

use crate::login::{LoginAttempt, LoginError};
use crate::{IdentityConfigError, VerifiedIdentity};

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EncryptedLogin {
    pub schema_version: u32,
    pub key_id: String,
    pub state_hash: [u8; 32],
    pub browser_hash: [u8; 32],
    pub expires_at: u64,
    pub nonce: [u8; 12],
    pub ciphertext: Vec<u8>,
}

impl EncryptedLogin {
    pub fn validate(&self) -> Result<(), LoginError> {
        self.aad().map(|_| ())
    }

    fn aad(&self) -> Result<Vec<u8>, LoginError> {
        if self.schema_version != 1
            || self.key_id.is_empty()
            || self.key_id.len() > 64
            || self.ciphertext.len() > 8192
        {
            return Err(LoginError::Transaction);
        }
        serde_json::to_vec(&(
            "zuno.enterprise.login",
            self.schema_version,
            &self.key_id,
            self.state_hash,
            self.browser_hash,
            self.expires_at,
        ))
        .map_err(|_| LoginError::Transaction)
    }
}

pub struct LoginStateCipher {
    current_key: String,
    keys: BTreeMap<String, RandomizedNonceKey>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Payload {
    state: String,
    nonce: String,
    verifier: String,
    configuration: [u8; 32],
}

impl LoginStateCipher {
    /// Host secret resolution supplies 32-byte keys. Retired keys are retained
    /// explicitly only while their short-lived transactions can still return.
    pub fn new(current_key: String, keys: BTreeMap<String, Vec<u8>>) -> Result<Self, LoginError> {
        if !keys.contains_key(&current_key) || keys.is_empty() || keys.len() > 8 {
            return Err(IdentityConfigError("invalid login encryption keyring").into());
        }
        let keys = keys
            .into_iter()
            .map(|(id, key)| {
                if id.is_empty()
                    || id.len() > 64
                    || !id
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte))
                    || key.len() != 32
                {
                    return Err(LoginError::Configuration(IdentityConfigError(
                        "invalid login encryption key",
                    )));
                }
                let key = RandomizedNonceKey::new(&AES_256_GCM, &key)
                    .map_err(|_| LoginError::Transaction)?;
                Ok((id, key))
            })
            .collect::<Result<_, _>>()?;
        Ok(Self { current_key, keys })
    }

    pub fn seal(&self, attempt: LoginAttempt) -> Result<EncryptedLogin, LoginError> {
        let mut record = EncryptedLogin {
            schema_version: 1,
            key_id: self.current_key.clone(),
            state_hash: Sha256::digest(attempt.state.expose().as_bytes()).into(),
            browser_hash: attempt.browser_binding,
            expires_at: attempt.expires_at,
            nonce: [0; 12],
            ciphertext: Vec::new(),
        };
        let aad = record.aad()?;
        let mut payload = serde_json::to_vec(&Payload {
            state: attempt.state.expose().to_owned(),
            nonce: attempt.nonce.expose().to_owned(),
            verifier: attempt.verifier.expose().to_owned(),
            configuration: attempt.configuration,
        })
        .map_err(|_| LoginError::Transaction)?;
        if payload.len() > 4096 {
            return Err(LoginError::Transaction);
        }
        let nonce = self.keys[&self.current_key]
            .seal_in_place_append_tag(Aad::from(aad), &mut payload)
            .map_err(|_| LoginError::Transaction)?;
        record.nonce.copy_from_slice(nonce.as_ref());
        record.ciphertext = payload;
        Ok(record)
    }

    pub fn open(
        &self,
        mut record: EncryptedLogin,
        state: &str,
        browser_binding: &str,
        now: u64,
    ) -> Result<LoginAttempt, LoginError> {
        if now >= record.expires_at
            || aws_lc_rs::constant_time::verify_slices_are_equal(
                &Sha256::digest(state.as_bytes()),
                &record.state_hash,
            )
            .is_err()
            || aws_lc_rs::constant_time::verify_slices_are_equal(
                &Sha256::digest(browser_binding.as_bytes()),
                &record.browser_hash,
            )
            .is_err()
        {
            return Err(LoginError::Transaction);
        }
        let aad = record.aad()?;
        let key = self
            .keys
            .get(&record.key_id)
            .ok_or(LoginError::Transaction)?;
        let nonce =
            Nonce::try_assume_unique_for_key(&record.nonce).map_err(|_| LoginError::Transaction)?;
        let plaintext = key
            .open_in_place(nonce, Aad::from(aad), &mut record.ciphertext)
            .map_err(|_| LoginError::Transaction)?;
        let payload: Payload =
            serde_json::from_slice(plaintext).map_err(|_| LoginError::Transaction)?;
        if payload.state != state
            || payload.nonce.is_empty()
            || payload.nonce.len() > 256
            || !(43..=128).contains(&payload.verifier.len())
        {
            return Err(LoginError::Transaction);
        }
        Ok(LoginAttempt {
            state: Secret::new(payload.state),
            nonce: Secret::new(payload.nonce),
            verifier: Secret::new(payload.verifier),
            configuration: payload.configuration,
            browser_binding: record.browser_hash,
            expires_at: record.expires_at,
        })
    }
}

/// The provider consumes a transaction atomically before the token exchange.
/// Invalid browser bindings do not consume another browser's login attempt.
#[async_trait]
pub trait LoginTransactionStore: Send + Sync {
    async fn insert(&self, login: EncryptedLogin) -> Result<(), LoginError>;
    async fn take(
        &self,
        state_hash: [u8; 32],
        browser_hash: [u8; 32],
        now: u64,
    ) -> Result<Option<EncryptedLogin>, LoginError>;
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BrowserSessionRecord {
    pub token_hash: [u8; 32],
    pub issuer: String,
    pub tenant_id: zuno_types::identity::TenantId,
    pub principal_id: zuno_types::identity::PrincipalId,
    pub client_id: zuno_types::identity::ClientId,
    pub oauth_client_id: String,
    pub expires_at: u64,
}

impl BrowserSessionRecord {
    pub fn from_identity(
        token_hash: [u8; 32],
        identity: &VerifiedIdentity,
        expires_at: u64,
    ) -> Result<Self, LoginError> {
        if identity.kind() != crate::VerifiedIdentityKind::DelegatedUser
            || expires_at > identity.expires_at_seconds()
        {
            return Err(LoginError::Transaction);
        }
        Ok(Self {
            token_hash,
            issuer: identity.issuer().to_owned(),
            tenant_id: identity.tenant_id().clone(),
            principal_id: identity.principal_id().clone(),
            client_id: identity.client_id().clone(),
            oauth_client_id: identity.oauth_client_id().to_owned(),
            expires_at,
        })
    }
}

/// Installed only in the BFF. No cookie, access token or refresh token is stored
/// in a browser session row; the row holds a random credential's digest.
#[async_trait]
pub trait BrowserSessionStore: Send + Sync {
    async fn create(&self, session: BrowserSessionRecord) -> Result<(), LoginError>;
    async fn lookup(
        &self,
        token_hash: [u8; 32],
        now: u64,
    ) -> Result<Option<BrowserSessionRecord>, LoginError>;
    async fn revoke(&self, token_hash: [u8; 32]) -> Result<(), LoginError>;
}
