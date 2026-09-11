//! Enterprise identity verification, separate from provider credential storage.
//!
//! Only a verified API access token can produce [`VerifiedIdentity`]. Identity
//! is not organization authorization or an approval: hosts must consult current
//! policy before constructing an application backend, admitting a Job, or
//! executing an operation. Never deserialize a `PrincipalScope` as authentication.

mod authority;
pub mod browser_login;
mod config;
pub mod gateway;
mod id_token;
mod introspection;
mod jwt;
mod keys;
pub mod login;
mod login_options;
pub mod login_state;
mod oauth2;
mod verifier;
pub mod worker;

pub use authority::OAuth2Authority;
pub use config::{ActorPolicy, EntraConfig, IdentityConfigError};
pub use id_token::{OidcIdTokenVerifier, VerifiedIdToken};
pub use introspection::{
    HttpTokenIntrospector, IntrospectionClientAuth, OAuth2IntrospectionConfig,
    OAuth2IntrospectionVerifier, TokenIntrospector,
};
pub use keys::{KeyCacheOptions, OidcKeySource, SigningKeyCache, SigningKeySource};
pub use login_options::OidcLoginOptions;
pub use oauth2::{
    ClaimRequirement, OAuth2ClaimsPolicy, OAuth2JwtConfig, OAuth2JwtProfile, OAuth2JwtVerifier,
    OAuth2PrincipalKind,
};
pub use verifier::{
    AccessTokenVerifier, EntraVerifier, IdentityError, TokenRejection, VerifiedIdentity,
    VerifiedIdentityKind,
};

#[cfg(test)]
mod tests;
