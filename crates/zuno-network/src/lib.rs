//! Shared outbound HTTP policy.
//!
//! Zuno's session-owned HTTP clients use [`ProxyPolicy::Environment`]. Reqwest
//! resolves `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`, and `NO_PROXY` (including
//! their lowercase aliases) when the client is built. Keeping construction
//! behind this crate makes that behavior a product contract rather than an
//! accidental property of whichever provider happened to instantiate reqwest.
//!
//! [`ProxyPolicy::Direct`] is reserved for local control-plane traffic and
//! cloud metadata services whose credentials must never be forwarded to an
//! ambient proxy.

use reqwest::{Client, ClientBuilder};

/// How an outbound HTTP client treats process proxy configuration.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ProxyPolicy {
    /// Honor the standard proxy and bypass environment variables.
    #[default]
    Environment,
    /// Never use a proxy, even when proxy variables are present.
    Direct,
}

impl ProxyPolicy {
    /// Start a reqwest client builder with this policy applied.
    pub fn client_builder(self) -> ClientBuilder {
        match self {
            Self::Environment => Client::builder(),
            Self::Direct => Client::builder().no_proxy(),
        }
    }

    /// Build a client with this policy.
    pub fn build_client(self) -> reqwest::Result<Client> {
        self.client_builder().build()
    }
}

/// Start a session-network client builder that honors proxy environment variables.
pub fn client_builder() -> ClientBuilder {
    ProxyPolicy::Environment.client_builder()
}

/// Construct a session-network client that honors proxy environment variables.
///
/// This mirrors [`reqwest::Client::new`]: TLS initialization failure is a
/// process-level startup defect for callers whose public constructors are
/// intentionally infallible.
#[must_use]
pub fn client() -> Client {
    ProxyPolicy::Environment
        .build_client()
        .expect("session HTTP client must initialize")
}

/// Start a client builder that cannot use an ambient proxy.
pub fn direct_client_builder() -> ClientBuilder {
    ProxyPolicy::Direct.client_builder()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_is_the_session_environment_policy() {
        assert_eq!(ProxyPolicy::default(), ProxyPolicy::Environment);
    }

    #[test]
    fn shipped_reqwest_features_accept_socks_proxy_schemes() {
        for proxy in [
            "socks4://127.0.0.1:1080",
            "socks4a://127.0.0.1:1080",
            "socks5://127.0.0.1:1080",
            "socks5h://127.0.0.1:1080",
        ] {
            let proxy = reqwest::Proxy::all(proxy).expect("supported SOCKS proxy URL");
            reqwest::Client::builder()
                .proxy(proxy)
                .build()
                .expect("SOCKS-enabled client");
        }
    }
}
