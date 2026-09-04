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

use std::time::Duration;

use reqwest::{Client, ClientBuilder};

pub use public_http::{
    DiagnosticEndpoint, HostResolver, PublicHttpClient, PublicHttpError, PublicHttpPolicy,
    PublicHttpResponse, PublicTarget, SystemHostResolver, is_public_ip,
};

/// Connect-phase default for every session client.
///
/// A peer that answers slowly is bounded by whatever request deadline its caller applies,
/// but a peer that silently drops the SYN is bounded by nothing: `zuno auth login` behind
/// a DROP firewall hangs until the user interrupts it. A connect deadline cannot truncate
/// an established response, so it is the one timeout this shared constructor can carry
/// without retiming long-lived streams (see `zuno_llm::http` for why request and read
/// deadlines stay with the caller).
///
/// It is a default, not an enforced ceiling. This crate hands out a `ClientBuilder`, and
/// `connect_timeout` is last-write-wins, so a consumer that calls it again replaces this
/// value: `zuno-provider-bedrock` (2s), `zuno-provider-google` (5s), and
/// `zuno session prune` (500ms) all tighten it today, and nothing here forces that
/// direction. Making the direction structural the way
/// `PublicHttpClient::with_establish_timeout` does would mean this crate stops returning a
/// raw builder and owns a clamping setter instead, which is a change in every consumer
/// rather than in this constant.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

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
        let builder = match self {
            Self::ProcessEnvironment => Client::builder(),
            Self::Direct(purpose) => {
                // The bypass is a security promise (metadata credentials must not reach an
                // ambient proxy), so record which purpose claimed it rather than dropping
                // the declaration on the floor.
                //
                // The credential-bearing purpose is recorded at `info!` because the shipped
                // default level is `Info`: at `debug!` the one bypass that hands a freshly
                // minted cloud token to a non-proxied route would be invisible in every
                // default install, which is not an auditable security promise. A loopback
                // control-plane probe carries no credential a proxy could see and is built
                // on ordinary command paths, so it stays at `debug!` rather than adding a
                // line to every `zuno session prune` run.
                match purpose {
                    DirectPurpose::CloudMetadata => {
                        tracing::info!(?purpose, "session HTTP client bypasses the process proxy");
                    }
                    DirectPurpose::LoopbackControlPlane => {
                        tracing::debug!(?purpose, "session HTTP client bypasses the process proxy");
                    }
                }
                Client::builder().no_proxy()
            }
        };
        builder.connect_timeout(CONNECT_TIMEOUT)
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
    use std::fmt;
    use std::sync::{Arc, Mutex};

    /// A subscriber that records level and fields, so the crate needs no test-only
    /// dependency to assert what a default install would actually see.
    #[derive(Clone, Default)]
    struct CapturedEvents(Arc<Mutex<Vec<(tracing::Level, String)>>>);

    impl CapturedEvents {
        fn events(&self) -> Vec<(tracing::Level, String)> {
            self.0.lock().expect("captured events").clone()
        }
    }

    struct FieldText<'a>(&'a mut String);

    impl tracing::field::Visit for FieldText<'_> {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn fmt::Debug) {
            use fmt::Write as _;
            let _ = write!(self.0, "{}={value:?} ", field.name());
        }
    }

    impl tracing::Subscriber for CapturedEvents {
        fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
            true
        }

        fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }

        fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}

        fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

        fn event(&self, event: &tracing::Event<'_>) {
            let mut fields = String::new();
            event.record(&mut FieldText(&mut fields));
            self.0
                .lock()
                .expect("captured events")
                .push((*event.metadata().level(), fields));
        }

        fn enter(&self, _span: &tracing::span::Id) {}

        fn exit(&self, _span: &tracing::span::Id) {}
    }

    #[test]
    fn default_policy_is_the_session_environment_policy() {
        assert_eq!(
            SessionNetworkPolicy::default(),
            SessionNetworkPolicy::ProcessEnvironment
        );
    }

    /// Every policy reaches the shared tail that applies the connect default.
    ///
    /// This pins that no policy arm can return a builder without the deadline. It does not
    /// pin a ceiling: a consumer that calls `connect_timeout` again on the builder it
    /// receives replaces the value, and that is deliberately outside what this crate can
    /// observe.
    #[test]
    fn every_policy_carries_the_connect_default() {
        for policy in [
            SessionNetworkPolicy::ProcessEnvironment,
            SessionNetworkPolicy::Direct(DirectPurpose::LoopbackControlPlane),
            SessionNetworkPolicy::Direct(DirectPurpose::CloudMetadata),
        ] {
            let rendered = format!("{:?}", policy.client_builder());
            assert!(
                rendered.contains("connect_timeout: 30s"),
                "{policy:?} must reach the shared tail that applies the connect default: \
                 {rendered}"
            );
        }
    }

    #[test]
    fn a_credential_bearing_bypass_is_visible_at_the_shipped_default_level() {
        let captured = CapturedEvents::default();
        tracing::subscriber::with_default(captured.clone(), || {
            let _metadata = SessionNetworkPolicy::Direct(DirectPurpose::CloudMetadata)
                .client_builder()
                .build()
                .expect("metadata client");
            let _loopback = SessionNetworkPolicy::Direct(DirectPurpose::LoopbackControlPlane)
                .client_builder()
                .build()
                .expect("loopback client");
            let _session = SessionNetworkPolicy::ProcessEnvironment
                .client_builder()
                .build()
                .expect("session client");
        });
        // Only events at or above `Info` are asserted on. The shipped default level is
        // `Info` (`zuno_observability::LogLevel::default()`), so that is exactly the set an
        // operator sees without changing configuration and restarting - and it is also the
        // only set a shared test binary can observe deterministically, because a scoped
        // subscriber does not lower the process-wide max level for a `debug!` callsite that
        // another test already reached with no subscriber installed.
        let visible = captured
            .events()
            .into_iter()
            .filter(|(level, _)| *level <= tracing::Level::INFO)
            .collect::<Vec<_>>();
        assert_eq!(
            visible.len(),
            1,
            "exactly one of the three policies declares a credential-bearing bypass: {visible:?}"
        );
        let (level, fields) = &visible[0];
        assert_eq!(*level, tracing::Level::INFO);
        assert!(
            fields.contains("CloudMetadata"),
            "the recorded bypass must name the purpose that claimed it: {fields}"
        );
        assert!(
            !fields.contains("LoopbackControlPlane"),
            "a loopback probe carries no credential a proxy could see, so it must not add a \
             line to every command that makes one: {fields}"
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
