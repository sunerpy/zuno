use std::sync::Arc;

use jsonwebtoken::{Algorithm, Validation, decode, decode_header, errors::ErrorKind};
use serde::de::DeserializeOwned;

use crate::keys::valid_kid;
use crate::{IdentityError, OAuth2Authority, SigningKeyCache, TokenRejection};

pub(crate) const MAX_ACCESS_TOKEN_BYTES: usize = 32 * 1024;

#[derive(Clone, Copy)]
pub(crate) enum JwtHeaderType {
    ProviderJwt,
    Rfc9068,
}

/// Cryptographic JWT access-token validation independent of identity mapping.
pub(crate) struct JwtValidator {
    authority: OAuth2Authority,
    audience: String,
    header_type: JwtHeaderType,
    clock_skew_seconds: u64,
    keys: Arc<SigningKeyCache>,
}

impl JwtValidator {
    pub(crate) fn new(
        authority: OAuth2Authority,
        audience: String,
        header_type: JwtHeaderType,
        clock_skew_seconds: u64,
        keys: Arc<SigningKeyCache>,
    ) -> Result<Self, IdentityError> {
        if &authority != keys.authority() {
            return Err(crate::IdentityConfigError(
                "key cache has a different issuer or trust policy",
            )
            .into());
        }
        Ok(Self {
            authority,
            audience,
            header_type,
            clock_skew_seconds,
            keys,
        })
    }

    pub(crate) async fn validate<T: DeserializeOwned>(
        &self,
        token: &str,
    ) -> Result<T, IdentityError> {
        validate_token_size(token)?;
        let header = decode_header(token).map_err(|_| TokenRejection::InvalidToken)?;
        let valid_type = match self.header_type {
            JwtHeaderType::ProviderJwt => header.typ.as_deref() == Some("JWT"),
            JwtHeaderType::Rfc9068 => {
                matches!(header.typ.as_deref(), Some("at+jwt" | "application/at+jwt"))
            }
        };
        if header.alg != Algorithm::RS256
            || !valid_type
            || header.crit.as_ref().is_some_and(|crit| !crit.is_empty())
            || header.jku.is_some()
            || header.jwk.is_some()
            || header.x5u.is_some()
            || header.enc.is_some()
            || header.zip.is_some()
            || header.extras.inner().contains_key("b64")
        {
            return Err(TokenRejection::UnsupportedToken.into());
        }
        let kid = header
            .kid
            .filter(|kid| valid_kid(kid))
            .ok_or(TokenRejection::InvalidToken)?;
        let key = self.keys.key(&kid).await?;
        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_required_spec_claims(&["exp", "aud", "iss", "sub"]);
        validation.set_audience(&[&self.audience]);
        validation.set_issuer(&[self.authority.issuer()]);
        validation.validate_nbf = true;
        validation.leeway = self.clock_skew_seconds;
        Ok(decode::<T>(token, &key, &validation)
            .map_err(|error| match error.kind() {
                ErrorKind::ExpiredSignature => TokenRejection::Expired,
                ErrorKind::ImmatureSignature => TokenRejection::NotYetValid,
                ErrorKind::InvalidIssuer => TokenRejection::Issuer,
                ErrorKind::InvalidAudience => TokenRejection::Audience,
                _ => TokenRejection::InvalidToken,
            })?
            .claims)
    }
}

pub(crate) fn validate_token_size(token: &str) -> Result<(), IdentityError> {
    if token.is_empty()
        || token.len() > MAX_ACCESS_TOKEN_BYTES
        || !token.is_ascii()
        || token
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
    {
        return Err(TokenRejection::InvalidToken.into());
    }
    Ok(())
}
