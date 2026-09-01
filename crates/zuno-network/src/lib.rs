//! Shared outbound HTTP policy.
//!
//! Zuno's session-owned HTTP clients use [`SessionNetworkPolicy::ProcessEnvironment`]. Reqwest
//! resolves `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`, and `NO_PROXY` (including
//! their lowercase aliases) when the client is built. Keeping construction
//! behind this crate makes that behavior a product contract rather than an
//! accidental property of whichever provider happened to instantiate reqwest.
//!
//! Direct traffic is intentionally not a boolean escape hatch. A caller must
//! declare the narrow control-plane or metadata purpose that makes bypassing the
//! process proxy correct.

mod proxy_transport;
mod public_http;

use reqwest::{Client, ClientBuilder};

pub use public_http::{
    DiagnosticEndpoint, HostResolver, PublicHttpClient, PublicHttpError, PublicHttpPolicy,
    PublicHttpResponse, PublicTarget, SystemHostResolver, is_public_ip,
};

/// The only supported reasons to bypass process proxy configuration.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DirectPurpose {
    /// Loopback/local control-plane probes owned by the same Zuno process.
    #[default]
    LoopbackControlPlane,
    /// Cloud instance metadata endpoints whose credentials must not reach a proxy.
    CloudMetadata,
}

/// Unified policy for one session-owned HTTP client.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SessionNetworkPolicy {
    /// Honor `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`, and `NO_PROXY`.
    #[default]
    ProcessEnvironment,
    /// Bypass proxies for one explicitly-declared narrow purpose.
    Direct(DirectPurpose),
}

impl SessionNetworkPolicy {
    /// Start a reqwest client builder with this policy applied.
    pub fn client_builder(self) -> ClientBuilder {
        match self {
            Self::ProcessEnvironment => Client::builder(),
            Self::Direct(_) => Client::builder().no_proxy(),
        }
    }

    /// Build a client with this policy.
    pub fn build_client(self) -> reqwest::Result<Client> {
        self.client_builder().build()
    }
}

/// Start a session-network client builder that honors proxy environment variables.
pub fn client_builder() -> ClientBuilder {
    SessionNetworkPolicy::ProcessEnvironment.client_builder()
}

/// Construct a session-network client that honors proxy environment variables.
///
/// This mirrors [`reqwest::Client::new`]: TLS initialization failure is a
/// process-level startup defect for callers whose public constructors are
/// intentionally infallible.
#[must_use]
pub fn client() -> Client {
    SessionNetworkPolicy::ProcessEnvironment
        .build_client()
        .expect("session HTTP client must initialize")
}

/// Start a client builder that cannot use an ambient proxy for the declared purpose.
pub fn direct_client_builder(purpose: DirectPurpose) -> ClientBuilder {
    SessionNetworkPolicy::Direct(purpose).client_builder()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_is_the_session_environment_policy() {
        assert_eq!(
            SessionNetworkPolicy::default(),
            SessionNetworkPolicy::ProcessEnvironment
        );
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
