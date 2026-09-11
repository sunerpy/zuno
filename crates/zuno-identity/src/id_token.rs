//! OIDC authentication proof, separate from API access-token authorization.

use std::sync::Arc;

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::jwt::{JwtHeaderType, JwtValidator};
use crate::{IdentityConfigError, IdentityError, OAuth2Authority, SigningKeyCache, TokenRejection};

/// A browser login must still map its API token through AccessTokenVerifier.
/// An ID token is never accepted as an API bearer credential.
pub struct OidcIdTokenVerifier {
    jwt: JwtValidator,
    issuer: String,
    client_id: String,
    clock_skew_seconds: u64,
}

#[derive(Debug, Clone)]
pub struct VerifiedIdToken {
    issuer: String,
    subject: String,
    expires_at_seconds: u64,
}

impl VerifiedIdToken {
    pub fn issuer(&self) -> &str {
        &self.issuer
    }
    pub fn subject(&self) -> &str {
        &self.subject
    }
    pub fn expires_at_seconds(&self) -> u64 {
        self.expires_at_seconds
    }
}

impl OidcIdTokenVerifier {
    pub fn new(
        authority: OAuth2Authority,
        client_id: String,
        clock_skew_seconds: u64,
        keys: Arc<SigningKeyCache>,
    ) -> Result<Self, IdentityError> {
        if client_id.is_empty()
            || client_id.len() > 256
            || client_id.chars().any(char::is_control)
            || clock_skew_seconds > 120
        {
            return Err(IdentityConfigError("invalid OIDC client or clock skew").into());
        }
        let issuer = authority.issuer().to_owned();
        let jwt = JwtValidator::new(
            authority,
            client_id.clone(),
            JwtHeaderType::OidcIdToken,
            clock_skew_seconds,
            keys,
        )?;
        Ok(Self {
            jwt,
            issuer,
            client_id,
            clock_skew_seconds,
        })
    }

    pub async fn verify(
        &self,
        id_token: &str,
        expected_nonce: &str,
        access_token: Option<&str>,
        max_authentication_age_seconds: Option<u64>,
    ) -> Result<VerifiedIdToken, IdentityError> {
        if expected_nonce.is_empty() || expected_nonce.len() > 256 {
            return Err(TokenRejection::Claims.into());
        }
        let claims: IdClaims = self.jwt.validate(id_token).await?;
        let now = jsonwebtoken::get_current_timestamp();
        let audiences = match &claims.aud {
            Audience::One(value) => std::slice::from_ref(value),
            Audience::Many(values) => values.as_slice(),
        };
        if claims.iss != self.issuer
            || claims.sub.is_empty()
            || claims.sub.len() > 255
            || audiences.is_empty()
            || audiences.len() > 16
            || audiences.iter().any(|value| value.is_empty())
            || !audiences.contains(&self.client_id)
            || claims.exp <= claims.iat
            || claims.iat > now.saturating_add(self.clock_skew_seconds)
            || (audiences.len() > 1 && claims.azp.as_deref() != Some(self.client_id.as_str()))
            || claims
                .azp
                .as_ref()
                .is_some_and(|azp| azp != &self.client_id)
            || aws_lc_rs::constant_time::verify_slices_are_equal(
                expected_nonce.as_bytes(),
                claims.nonce.as_bytes(),
            )
            .is_err()
        {
            return Err(TokenRejection::Claims.into());
        }
        if let Some(max_age) = max_authentication_age_seconds {
            let authenticated = claims.auth_time.ok_or(TokenRejection::Claims)?;
            if authenticated > now.saturating_add(self.clock_skew_seconds)
                || now
                    > authenticated
                        .saturating_add(max_age)
                        .saturating_add(self.clock_skew_seconds)
            {
                return Err(TokenRejection::Claims.into());
            }
        }
        if let Some(expected) = claims.at_hash {
            let token = access_token.ok_or(TokenRejection::Claims)?;
            let digest = Sha256::digest(token.as_bytes());
            let actual = URL_SAFE_NO_PAD.encode(&digest[..digest.len() / 2]);
            if aws_lc_rs::constant_time::verify_slices_are_equal(
                expected.as_bytes(),
                actual.as_bytes(),
            )
            .is_err()
            {
                return Err(TokenRejection::Claims.into());
            }
        }
        Ok(VerifiedIdToken {
            issuer: claims.iss,
            subject: claims.sub,
            expires_at_seconds: claims.exp,
        })
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Audience {
    One(String),
    Many(Vec<String>),
}

#[derive(Deserialize)]
struct IdClaims {
    iss: String,
    sub: String,
    aud: Audience,
    exp: u64,
    iat: u64,
    nonce: String,
    azp: Option<String>,
    at_hash: Option<String>,
    auth_time: Option<u64>,
}
