use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use jsonwebtoken::DecodingKey;
use reqwest::{Client, Url};
use serde::Deserialize;
use tokio::sync::{Mutex, RwLock};
use tokio::time::Instant;

use crate::authority::KeyIssuerRule;
use crate::{IdentityConfigError, IdentityError, OAuth2Authority, TokenRejection};

const MAX_DOCUMENT_BYTES: usize = 512 * 1024;
const MAX_KEYS: usize = 64;

/// Trusted metadata transport. Hosts may inject an enterprise transport; clients
/// cannot submit signing keys. All returned bytes still undergo key validation.
#[async_trait]
pub trait SigningKeySource: Send + Sync {
    async fn fetch(&self) -> Result<Vec<u8>, IdentityError>;
}

/// Fixed-authority, bounded HTTPS discovery and JWKS retrieval.
pub struct OidcKeySource {
    client: Client,
    authority: OAuth2Authority,
}

impl OidcKeySource {
    pub fn new(authority: OAuth2Authority) -> Result<Self, IdentityError> {
        let client = Client::builder()
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|_| IdentityError::KeysUnavailable)?;
        Ok(Self { client, authority })
    }

    async fn document(&self, url: Url) -> Result<Vec<u8>, IdentityError> {
        let mut response = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|_| IdentityError::KeysUnavailable)?;
        if !response.status().is_success()
            || response
                .content_length()
                .is_some_and(|length| length > MAX_DOCUMENT_BYTES as u64)
        {
            return Err(IdentityError::KeysUnavailable);
        }
        let mut data = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| IdentityError::KeysUnavailable)?
        {
            if data.len().saturating_add(chunk.len()) > MAX_DOCUMENT_BYTES {
                return Err(IdentityError::KeysUnavailable);
            }
            data.extend_from_slice(&chunk);
        }
        Ok(data)
    }
}

#[derive(Deserialize)]
struct Discovery {
    issuer: String,
    jwks_uri: String,
}

#[async_trait]
impl SigningKeySource for OidcKeySource {
    async fn fetch(&self) -> Result<Vec<u8>, IdentityError> {
        if let Some(url) = &self.authority.jwks_url {
            return self.document(self.authority.check_jwks_url(url)?).await;
        }
        let discovery_url = Url::parse(&self.authority.discovery_url())
            .map_err(|_| IdentityError::KeysUnavailable)?;
        let discovery: Discovery = serde_json::from_slice(&self.document(discovery_url).await?)
            .map_err(|_| IdentityError::KeysUnavailable)?;
        if discovery.issuer != self.authority.issuer() {
            return Err(IdentityError::KeysUnavailable);
        }
        let url = self.authority.check_jwks_url(&discovery.jwks_uri)?;
        self.document(url).await
    }
}

#[derive(Clone, Copy)]
pub struct KeyCacheOptions {
    /// Expired keys are not served when discovery fails.
    pub max_age: Duration,
    /// One global bound on unknown-kid refreshes, not an unbounded negative map.
    pub refresh_interval: Duration,
}

impl Default for KeyCacheOptions {
    fn default() -> Self {
        Self {
            max_age: Duration::from_secs(3600),
            refresh_interval: Duration::from_secs(60),
        }
    }
}

#[derive(Default)]
struct KeyState {
    keys: BTreeMap<String, DecodingKey>,
    refreshed: Option<Instant>,
    attempted: Option<Instant>,
}

/// Single-flight, tenant-bound cache shared by API and worker validators.
pub struct SigningKeyCache {
    authority: OAuth2Authority,
    source: Arc<dyn SigningKeySource>,
    options: KeyCacheOptions,
    state: RwLock<KeyState>,
    refresh: Mutex<()>,
}

impl SigningKeyCache {
    pub fn new(
        authority: OAuth2Authority,
        source: Arc<dyn SigningKeySource>,
        options: KeyCacheOptions,
    ) -> Result<Self, IdentityConfigError> {
        if options.max_age < Duration::from_secs(60)
            || options.max_age > Duration::from_secs(24 * 3600)
            || options.refresh_interval < Duration::from_secs(10)
            || options.refresh_interval > Duration::from_secs(300)
            || options.refresh_interval > options.max_age
        {
            return Err(IdentityConfigError("invalid signing-key cache bounds"));
        }
        Ok(Self {
            authority,
            source,
            options,
            state: RwLock::new(KeyState::default()),
            refresh: Mutex::new(()),
        })
    }

    pub(crate) fn authority(&self) -> &OAuth2Authority {
        &self.authority
    }

    pub(crate) async fn key(&self, kid: &str) -> Result<DecodingKey, IdentityError> {
        {
            let state = self.state.read().await;
            if state
                .refreshed
                .is_some_and(|at| at.elapsed() < self.options.max_age)
                && let Some(key) = state.keys.get(kid)
            {
                return Ok(key.clone());
            }
        }
        // A slow refresh must not lock out callers using still-valid known keys.
        let _refresh = self.refresh.lock().await;
        let mut state = self.state.write().await;
        let now = Instant::now();
        let fresh = state
            .refreshed
            .is_some_and(|at| now.duration_since(at) < self.options.max_age);
        if fresh && let Some(key) = state.keys.get(kid) {
            return Ok(key.clone());
        }
        if state
            .attempted
            .is_some_and(|at| now.duration_since(at) < self.options.refresh_interval)
        {
            return Err(if fresh {
                TokenRejection::UnknownKey.into()
            } else {
                IdentityError::KeysUnavailable
            });
        }
        // Persist the cooldown before await, including cancellation and errors.
        state.attempted = Some(now);
        drop(state);
        let bytes = tokio::time::timeout(Duration::from_secs(20), self.source.fetch())
            .await
            .map_err(|_| IdentityError::KeysUnavailable)??;
        let keys = parse_keys(&bytes, &self.authority)?;
        let mut state = self.state.write().await;
        state.keys = keys;
        state.refreshed = Some(Instant::now());
        state
            .keys
            .get(kid)
            .cloned()
            .ok_or(TokenRejection::UnknownKey.into())
    }
}

#[derive(Deserialize)]
struct KeyDocument {
    keys: Vec<KeyEntry>,
}

#[derive(Deserialize)]
struct KeyEntry {
    kid: Option<String>,
    kty: String,
    #[serde(rename = "use")]
    usage: Option<String>,
    alg: Option<String>,
    key_ops: Option<Vec<String>>,
    issuer: Option<String>,
    n: Option<String>,
    e: Option<String>,
    #[serde(flatten)]
    extra: BTreeMap<String, serde_json::Value>,
}

fn parse_keys(
    bytes: &[u8],
    authority: &OAuth2Authority,
) -> Result<BTreeMap<String, DecodingKey>, IdentityError> {
    if bytes.len() > MAX_DOCUMENT_BYTES {
        return Err(IdentityError::KeysUnavailable);
    }
    let document: KeyDocument =
        serde_json::from_slice(bytes).map_err(|_| IdentityError::KeysUnavailable)?;
    if document.keys.is_empty() || document.keys.len() > MAX_KEYS {
        return Err(IdentityError::KeysUnavailable);
    }
    let mut keys = BTreeMap::new();
    for entry in document.keys {
        if entry.kty != "RSA"
            || entry.usage.as_deref().is_some_and(|usage| usage != "sig")
            || entry.alg.as_deref().is_some_and(|alg| alg != "RS256")
        {
            continue;
        }
        if entry
            .key_ops
            .as_ref()
            .is_some_and(|ops| ops.as_slice() != ["verify"])
            || ["d", "p", "q", "dp", "dq", "qi", "oth"]
                .iter()
                .any(|field| entry.extra.contains_key(*field))
        {
            return Err(IdentityError::KeysUnavailable);
        }
        // Microsoft publishes both tenant-specific and tenant-template issuers.
        let issuer_valid = match (&authority.key_issuer_rule, entry.issuer.as_deref()) {
            (KeyIssuerRule::Optional, None) => true,
            (_, Some(value)) if value == authority.issuer() => true,
            (KeyIssuerRule::Entra, Some("https://login.microsoftonline.com/{tenantid}/v2.0")) => {
                true
            }
            _ => false,
        };
        if !issuer_valid {
            return Err(IdentityError::KeysUnavailable);
        }
        let kid = entry.kid.filter(|kid| valid_kid(kid));
        let (Some(kid), Some(n), Some(e)) = (kid, entry.n, entry.e) else {
            return Err(IdentityError::KeysUnavailable);
        };
        let modulus = URL_SAFE_NO_PAD
            .decode(&n)
            .map_err(|_| IdentityError::KeysUnavailable)?;
        let exponent = URL_SAFE_NO_PAD
            .decode(&e)
            .map_err(|_| IdentityError::KeysUnavailable)?;
        if !(256..=1024).contains(&modulus.len())
            || modulus[0] & 0x80 == 0
            || exponent.is_empty()
            || exponent.len() > 4
            || exponent[0] == 0
            || exponent.last().is_none_or(|value| value & 1 == 0)
            || (exponent.len() == 1 && exponent[0] < 3)
        {
            return Err(IdentityError::KeysUnavailable);
        }
        let key =
            DecodingKey::from_rsa_components(&n, &e).map_err(|_| IdentityError::KeysUnavailable)?;
        if keys.insert(kid, key).is_some() {
            return Err(IdentityError::KeysUnavailable);
        }
    }
    if keys.is_empty() {
        return Err(IdentityError::KeysUnavailable);
    }
    Ok(keys)
}

pub(crate) fn valid_kid(kid: &str) -> bool {
    !kid.is_empty()
        && kid.len() <= 256
        && kid
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
}

#[cfg(test)]
mod tests {
    use crate::{ActorPolicy, EntraConfig};

    #[test]
    fn discovery_cannot_redirect_key_fetches_to_another_origin_or_tenant() {
        let config = EntraConfig::new(
            "11111111-1111-4111-8111-111111111111",
            "22222222-2222-4222-8222-222222222222",
            ["33333333-3333-4333-8333-333333333333".to_owned()].into(),
            ActorPolicy::Workload {
                required_roles: ["Worker.Run".to_owned()].into(),
            },
        )
        .unwrap();
        assert!(
            config
                .authority()
                .check_jwks_url("https://login.microsoftonline.com/common/discovery/v2.0/keys")
                .is_ok()
        );
        for invalid in [
            "http://login.microsoftonline.com/common/discovery/v2.0/keys",
            "https://login.microsoftonline.com.attacker.test/common/discovery/v2.0/keys",
            "https://127.0.0.1/common/discovery/v2.0/keys",
            "https://login.microsoftonline.com:8443/common/discovery/v2.0/keys",
            "https://user@login.microsoftonline.com/common/discovery/v2.0/keys",
            "https://login.microsoftonline.com/common/discovery/v2.0/keys?appid=other",
            "https://login.microsoftonline.com/common/discovery/v2.0/keys#fragment",
            "https://login.microsoftonline.com/other/discovery/v2.0/keys",
        ] {
            assert!(
                config.authority().check_jwks_url(invalid).is_err(),
                "{invalid}"
            );
        }
    }
}
