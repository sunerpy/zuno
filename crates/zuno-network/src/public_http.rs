//! SSRF-safe HTTP GET transport for public internet resources.
//!
//! The security boundary is deliberately below individual tools. A caller supplies a
//! syntactically validated [`PublicTarget`]; this service resolves every hop, rejects the
//! whole answer set when any address is non-public, pins accepted direct connections, honors
//! the process proxy policy, and performs redirects itself so every target is revalidated.

use async_trait::async_trait;
use bytes::Bytes;
use http_body_util::BodyExt as _;
use reqwest::StatusCode;
use reqwest::header::{AUTHORIZATION, COOKIE, HeaderMap, LOCATION, PROXY_AUTHORIZATION};
use std::fmt;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use url::{Host, Url};

use crate::proxy_transport::{RouteKind, SessionTransport};

const IPV4ONLY_ARPA: &str = "ipv4only.arpa";
const RFC_6052_PREFIX_LENGTHS: [usize; 6] = [32, 40, 48, 56, 64, 96];
const IPV4ONLY_SENTINELS: [Ipv4Addr; 2] =
    [Ipv4Addr::new(192, 0, 0, 170), Ipv4Addr::new(192, 0, 0, 171)];

/// Policy shared by every public-resource request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublicHttpPolicy {
    /// Maximum redirect hops before the request is abandoned.
    pub max_redirects: usize,
}

impl Default for PublicHttpPolicy {
    fn default() -> Self {
        Self { max_redirects: 5 }
    }
}

/// A URL representation safe to include in diagnostics.
///
/// User information, query parameters, and fragments are always removed. That keeps
/// provider keys and signed URLs out of error chains without reducing the endpoint to
/// an unactionable hostname.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct DiagnosticEndpoint(String);

impl DiagnosticEndpoint {
    /// Build a redacted endpoint from a parsed URL.
    #[must_use]
    pub fn from_url(url: &Url) -> Self {
        let mut redacted = url.clone();
        let _ = redacted.set_username("");
        let _ = redacted.set_password(None);
        redacted.set_query(None);
        redacted.set_fragment(None);
        Self(redacted.to_string())
    }

    /// The redacted endpoint text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for DiagnosticEndpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("DiagnosticEndpoint")
            .field(&self.0)
            .finish()
    }
}

impl fmt::Display for DiagnosticEndpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// A syntactically valid HTTP(S) target with no embedded credentials.
#[derive(Clone, PartialEq, Eq)]
pub struct PublicTarget {
    url: Url,
}

impl PublicTarget {
    /// Parse an HTTP(S) URL and reject user information or a missing host.
    pub fn parse(raw: &str) -> Result<Self, PublicHttpError> {
        let url = Url::parse(raw).map_err(|source| PublicHttpError::MalformedUrl { source })?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(PublicHttpError::UnsupportedScheme {
                scheme: url.scheme().to_owned(),
            });
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(PublicHttpError::Credentials {
                endpoint: DiagnosticEndpoint::from_url(&url),
            });
        }
        if url.host().is_none() {
            return Err(PublicHttpError::MissingHost);
        }
        if let Some(address) = literal_address(&url)
            && !is_public_ip(address)
        {
            return Err(PublicHttpError::BlockedAddress {
                endpoint: DiagnosticEndpoint::from_url(&url),
                address,
            });
        }
        Ok(Self { url })
    }

    /// The parsed URL used on the wire.
    #[must_use]
    pub fn url(&self) -> &Url {
        &self.url
    }

    /// A safe diagnostic rendering.
    #[must_use]
    pub fn diagnostic(&self) -> DiagnosticEndpoint {
        DiagnosticEndpoint::from_url(&self.url)
    }
}

impl fmt::Debug for PublicTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PublicTarget")
            .field("endpoint", &self.diagnostic())
            .finish()
    }
}

/// Resolver seam used to test mixed answers and DNS rebinding without live DNS.
#[async_trait]
pub trait HostResolver: Send + Sync {
    /// Resolve `host:port` into every address the connector may use.
    async fn resolve(&self, host: &str, port: u16) -> io::Result<Vec<SocketAddr>>;
}

/// Tokio's process resolver.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemHostResolver;

#[async_trait]
impl HostResolver for SystemHostResolver {
    async fn resolve(&self, host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
        Ok(tokio::net::lookup_host((host, port)).await?.collect())
    }
}

/// A redirect-aware public HTTP client using the session network policy.
#[derive(Clone)]
pub struct PublicHttpClient {
    resolver: Arc<dyn HostResolver>,
    policy: PublicHttpPolicy,
    transport: SessionTransport,
    #[cfg(test)]
    connector_addresses: Option<Vec<SocketAddr>>,
}

impl fmt::Debug for PublicHttpClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PublicHttpClient")
            .field("policy", &self.policy)
            .finish_non_exhaustive()
    }
}

impl Default for PublicHttpClient {
    fn default() -> Self {
        Self::new()
    }
}

impl PublicHttpClient {
    /// Construct the production transport.
    #[must_use]
    pub fn new() -> Self {
        Self::with_resolver(Arc::new(SystemHostResolver), PublicHttpPolicy::default())
    }

    /// Construct a transport over an injected resolver.
    #[must_use]
    pub fn with_resolver(resolver: Arc<dyn HostResolver>, policy: PublicHttpPolicy) -> Self {
        Self {
            resolver,
            policy,
            transport: SessionTransport::from_process(),
            #[cfg(test)]
            connector_addresses: None,
        }
    }

    /// The credential-free route currently selected for `target`.
    pub fn route_label(&self, target: &PublicTarget) -> Result<&'static str, PublicHttpError> {
        self.transport
            .route_label(target)
            .map(RouteKind::as_str)
            .map_err(|source| PublicHttpError::ProxyConfiguration {
                endpoint: target.diagnostic(),
                source,
            })
    }

    /// Perform one bounded-redirect GET.
    pub async fn get(
        &self,
        target: PublicTarget,
        mut headers: HeaderMap,
    ) -> Result<PublicHttpResponse, PublicHttpError> {
        let mut current = target;
        for followed in 0..=self.policy.max_redirects {
            let response = self.get_once(&current, headers.clone()).await?;
            if !is_redirect(response.status()) {
                return Ok(response);
            }
            if followed == self.policy.max_redirects {
                return Err(PublicHttpError::TooManyRedirects {
                    limit: self.policy.max_redirects,
                });
            }
            let location = response
                .headers()
                .get(LOCATION)
                .ok_or_else(|| PublicHttpError::MissingRedirectLocation {
                    endpoint: current.diagnostic(),
                })?
                .to_str()
                .map_err(|_| PublicHttpError::InvalidRedirectLocation {
                    endpoint: current.diagnostic(),
                })?;
            let next_url = current.url.join(location).map_err(|_| {
                PublicHttpError::InvalidRedirectLocation {
                    endpoint: current.diagnostic(),
                }
            })?;
            let next = PublicTarget::parse(next_url.as_str())?;
            if !same_origin(current.url(), next.url()) {
                headers.remove(AUTHORIZATION);
                headers.remove(PROXY_AUTHORIZATION);
                headers.remove(COOKIE);
            }
            current = next;
        }
        unreachable!("the redirect loop always returns at its configured bound")
    }

    async fn get_once(
        &self,
        target: &PublicTarget,
        headers: HeaderMap,
    ) -> Result<PublicHttpResponse, PublicHttpError> {
        let endpoint = target.diagnostic();
        let host = target.url.host_str().ok_or(PublicHttpError::MissingHost)?;
        let port = target
            .url
            .port_or_known_default()
            .ok_or(PublicHttpError::MissingPort)?;
        let addresses = match literal_address(&target.url) {
            Some(address) => vec![SocketAddr::new(address, port)],
            None => self.resolver.resolve(host, port).await.map_err(|source| {
                PublicHttpError::Resolve {
                    endpoint: endpoint.clone(),
                    source,
                }
            })?,
        };
        if addresses.is_empty() {
            return Err(PublicHttpError::NoAddresses { endpoint });
        }
        let nat64_prefixes = if addresses
            .iter()
            .any(|address| matches!(address.ip(), IpAddr::V6(_)))
        {
            self.discover_nat64_prefixes().await?
        } else {
            Vec::new()
        };
        for address in &addresses {
            if !is_public_ip(address.ip()) {
                return Err(PublicHttpError::BlockedAddress {
                    endpoint: endpoint.clone(),
                    address: address.ip(),
                });
            }
            if let IpAddr::V6(address) = address.ip()
                && let Some(translated) = translated_ipv4(address, &nat64_prefixes)
                && !is_public_ipv4(translated)
            {
                return Err(PublicHttpError::BlockedNat64Address {
                    endpoint: endpoint.clone(),
                    address,
                    translated,
                });
            }
        }

        #[cfg(test)]
        let connector_addresses = self.connector_addresses.as_deref().unwrap_or(&addresses);
        #[cfg(not(test))]
        let connector_addresses = addresses.as_slice();
        let route = self.transport.route_label(target).map_err(|source| {
            PublicHttpError::ProxyConfiguration {
                endpoint: target.diagnostic(),
                source,
            }
        })?;
        let (response, route) = self
            .transport
            .get(target, headers, &addresses, connector_addresses)
            .await
            .map_err(|source| PublicHttpError::Transport {
                endpoint: target.diagnostic(),
                route: route.as_str(),
                source,
            })?;
        Ok(PublicHttpResponse {
            response,
            endpoint: target.diagnostic(),
            route,
        })
    }

    async fn discover_nat64_prefixes(&self) -> Result<Vec<Nat64Prefix>, PublicHttpError> {
        let endpoint = DiagnosticEndpoint("dns://ipv4only.arpa".to_owned());
        let discovered = self
            .resolver
            .resolve(IPV4ONLY_ARPA, 0)
            .await
            .map_err(|source| PublicHttpError::Resolve { endpoint, source })?;
        Ok(nat64_prefixes(&discovered))
    }
}

/// Streaming response from the SSRF-safe public transport.
pub struct PublicHttpResponse {
    response: http::Response<hyper::body::Incoming>,
    endpoint: DiagnosticEndpoint,
    route: RouteKind,
}

impl fmt::Debug for PublicHttpResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PublicHttpResponse")
            .field("status", &self.status())
            .field("endpoint", &self.endpoint)
            .field("route", &self.route.as_str())
            .finish_non_exhaustive()
    }
}

impl PublicHttpResponse {
    /// HTTP response status.
    #[must_use]
    pub fn status(&self) -> StatusCode {
        self.response.status()
    }

    /// HTTP response headers.
    #[must_use]
    pub fn headers(&self) -> &HeaderMap {
        self.response.headers()
    }

    /// Credential-free endpoint for body diagnostics.
    #[must_use]
    pub fn endpoint(&self) -> &DiagnosticEndpoint {
        &self.endpoint
    }

    /// Credential-free route selected for this response.
    #[must_use]
    pub fn route(&self) -> &'static str {
        self.route.as_str()
    }

    /// Read the next data frame, skipping trailers.
    pub async fn chunk(&mut self) -> Result<Option<Bytes>, io::Error> {
        loop {
            let Some(frame) = self.response.body_mut().frame().await else {
                return Ok(None);
            };
            let frame = frame.map_err(io::Error::other)?;
            if let Ok(data) = frame.into_data() {
                return Ok(Some(data));
            }
        }
    }
}

/// Failures raised before a response body is consumed.
#[derive(Debug, thiserror::Error)]
pub enum PublicHttpError {
    /// The URL did not parse.
    #[error("could not parse target as a URL")]
    MalformedUrl {
        /// Parse failure.
        #[source]
        source: url::ParseError,
    },
    /// The scheme was not HTTP(S).
    #[error("public HTTP target must use http or https, got {scheme}")]
    UnsupportedScheme {
        /// Rejected scheme.
        scheme: String,
    },
    /// Embedded URL credentials are never permitted.
    #[error("public HTTP target {endpoint} must not contain credentials")]
    Credentials {
        /// Redacted endpoint.
        endpoint: DiagnosticEndpoint,
    },
    /// URL has no host.
    #[error("public HTTP target has no host")]
    MissingHost,
    /// HTTP(S) URL did not imply a usable port.
    #[error("public HTTP target has no usable port")]
    MissingPort,
    /// DNS lookup failed.
    #[error("could not resolve public HTTP target {endpoint}")]
    Resolve {
        /// Redacted endpoint.
        endpoint: DiagnosticEndpoint,
        /// Resolver error.
        #[source]
        source: io::Error,
    },
    /// DNS returned no connector addresses.
    #[error("public HTTP target {endpoint} resolved to no addresses")]
    NoAddresses {
        /// Redacted endpoint.
        endpoint: DiagnosticEndpoint,
    },
    /// Literal or resolved address is not public unicast.
    #[error("public HTTP target {endpoint} resolved to blocked address {address}")]
    BlockedAddress {
        /// Redacted endpoint.
        endpoint: DiagnosticEndpoint,
        /// Rejected address.
        address: IpAddr,
    },
    /// A public-looking IPv6 address translates to a blocked IPv4 destination.
    #[error(
        "public HTTP target {endpoint} resolved to NAT64 address {address} carrying blocked IPv4 address {translated}"
    )]
    BlockedNat64Address {
        /// Redacted endpoint.
        endpoint: DiagnosticEndpoint,
        /// Resolved IPv6 address.
        address: Ipv6Addr,
        /// Embedded IPv4 destination.
        translated: Ipv4Addr,
    },
    /// Proxy environment selected an invalid route.
    #[error("could not select the process proxy route for {endpoint}")]
    ProxyConfiguration {
        /// Redacted endpoint.
        endpoint: DiagnosticEndpoint,
        /// Malformed or unsupported proxy configuration.
        #[source]
        source: io::Error,
    },
    /// Sending the request failed.
    #[error("request to {endpoint} failed through {route}")]
    Transport {
        /// Redacted endpoint.
        endpoint: DiagnosticEndpoint,
        /// Credential-free route label.
        route: &'static str,
        /// Socket, proxy, TLS, or HTTP failure.
        #[source]
        source: io::Error,
    },
    /// Redirect response omitted Location.
    #[error("redirect from {endpoint} omitted a valid Location header")]
    MissingRedirectLocation {
        /// Redacted endpoint.
        endpoint: DiagnosticEndpoint,
    },
    /// Redirect Location was malformed.
    #[error("redirect from {endpoint} contained an invalid Location header")]
    InvalidRedirectLocation {
        /// Redacted endpoint.
        endpoint: DiagnosticEndpoint,
    },
    /// Redirect budget exhausted.
    #[error("redirect chain exceeded {limit} hops and was abandoned")]
    TooManyRedirects {
        /// Hop limit.
        limit: usize,
    },
}

impl PublicHttpError {
    /// Whether repeating the same target may succeed later.
    #[must_use]
    pub const fn is_transient(&self) -> bool {
        matches!(self, Self::Resolve { .. } | Self::Transport { .. })
    }
}

fn is_redirect(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::MOVED_PERMANENTLY
            | StatusCode::FOUND
            | StatusCode::SEE_OTHER
            | StatusCode::TEMPORARY_REDIRECT
            | StatusCode::PERMANENT_REDIRECT
    )
}

fn same_origin(left: &Url, right: &Url) -> bool {
    left.scheme() == right.scheme()
        && left.host() == right.host()
        && left.port_or_known_default() == right.port_or_known_default()
}

fn literal_address(url: &Url) -> Option<IpAddr> {
    match url.host()? {
        Host::Ipv4(address) => Some(IpAddr::V4(address)),
        Host::Ipv6(address) => Some(IpAddr::V6(address)),
        Host::Domain(_) => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Nat64Prefix {
    bytes: Vec<u8>,
    length: usize,
}

fn nat64_prefixes(addresses: &[SocketAddr]) -> Vec<Nat64Prefix> {
    let mut prefixes = Vec::new();
    for address in addresses {
        let IpAddr::V6(address) = address.ip() else {
            continue;
        };
        let bytes = address.octets();
        for length in RFC_6052_PREFIX_LENGTHS {
            let Some(embedded) = embedded_ipv4(&bytes, length) else {
                continue;
            };
            if !IPV4ONLY_SENTINELS.contains(&embedded) {
                continue;
            }
            let prefix = Nat64Prefix {
                bytes: bytes[..length / 8].to_vec(),
                length,
            };
            if !prefixes.contains(&prefix) {
                prefixes.push(prefix);
            }
        }
    }
    prefixes
}

fn translated_ipv4(address: Ipv6Addr, prefixes: &[Nat64Prefix]) -> Option<Ipv4Addr> {
    let bytes = address.octets();
    prefixes.iter().find_map(|prefix| {
        bytes
            .starts_with(&prefix.bytes)
            .then(|| embedded_ipv4(&bytes, prefix.length))
            .flatten()
    })
}

fn embedded_ipv4(bytes: &[u8; 16], prefix_length: usize) -> Option<Ipv4Addr> {
    let octets = if prefix_length == 96 {
        bytes[12..16].try_into().expect("fixed four-byte slice")
    } else {
        if bytes[8] != 0 {
            return None;
        }
        let prefix_bytes = prefix_length / 8;
        let before_reserved = 8 - prefix_bytes;
        let mut octets = [0_u8; 4];
        octets[..before_reserved]
            .copy_from_slice(&bytes[prefix_bytes..prefix_bytes + before_reserved]);
        octets[before_reserved..].copy_from_slice(&bytes[9..9 + (4 - before_reserved)]);
        octets
    };
    Some(Ipv4Addr::from(octets))
}

/// Return true only for public-unicast addresses suitable for arbitrary web access.
#[must_use]
pub fn is_public_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_public_ipv4(address),
        IpAddr::V6(address) => is_public_ipv6(address),
    }
}

fn is_public_ipv4(address: Ipv4Addr) -> bool {
    let octets = address.octets();
    if address.is_unspecified()
        || address.is_loopback()
        || address.is_private()
        || address.is_link_local()
        || address.is_broadcast()
        || address.is_documentation()
        || address.is_multicast()
    {
        return false;
    }
    octets[0] != 0
        && !(octets[0] == 100 && (64..=127).contains(&octets[1]))
        && octets[..3] != [192, 0, 0]
        && octets[..3] != [192, 88, 99]
        && !(octets[0] == 198 && (18..=19).contains(&octets[1]))
        && octets[0] < 240
}

fn is_public_ipv6(address: Ipv6Addr) -> bool {
    if let Some(mapped) = address.to_ipv4_mapped() {
        return is_public_ipv4(mapped);
    }
    let segments = address.segments();
    if segments[..6] == [0, 0, 0, 0, 0, 0] {
        return false;
    }
    if address.is_unspecified()
        || address.is_loopback()
        || address.is_multicast()
        || (segments[0] & 0xfe00) == 0xfc00
        || (segments[0] & 0xffc0) == 0xfe80
        || (segments[0] & 0xffc0) == 0xfec0
        || (segments[0] == 0x0100 && segments[1..4] == [0, 0, 0])
        || (segments[0] == 0x2001 && segments[1] == 0x0db8)
        || (segments[0] == 0x2001 && segments[1] == 0)
        || (segments[0] == 0x2001 && segments[1] == 2)
        || (segments[0] == 0x2001 && (segments[1] & 0xfff0) == 0x0010)
        || (segments[0] == 0x2001 && (segments[1] & 0xfff0) == 0x0020)
        || (segments[0] & 0xfff0) == 0x3ff0
        || (segments[0] == 0x0064 && segments[1] == 0xff9b && segments[2] == 1)
    {
        return false;
    }
    if segments[..6] == [0x0064, 0xff9b, 0, 0, 0, 0] {
        return is_public_ipv4(Ipv4Addr::new(
            (segments[6] >> 8) as u8,
            segments[6] as u8,
            (segments[7] >> 8) as u8,
            segments[7] as u8,
        ));
    }
    if segments[0] == 0x2002 {
        return is_public_ipv4(Ipv4Addr::new(
            (segments[1] >> 8) as u8,
            segments[1] as u8,
            (segments[2] >> 8) as u8,
            segments[2] as u8,
        ));
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{AUTHORIZATION, COOKIE, HeaderValue, PROXY_AUTHORIZATION};
    use std::collections::{HashMap, VecDeque};
    use std::sync::Mutex;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn target_rejects_credentials_and_private_literals() {
        assert!(matches!(
            PublicTarget::parse("https://user:secret@example.test/path?token=secret"),
            Err(PublicHttpError::Credentials { .. })
        ));
        assert!(matches!(
            PublicTarget::parse("http://127.0.0.1/private"),
            Err(PublicHttpError::BlockedAddress { .. })
        ));
        assert!(matches!(
            PublicTarget::parse("http://[::1]/private"),
            Err(PublicHttpError::BlockedAddress { .. })
        ));
    }

    #[test]
    fn diagnostics_remove_query_fragment_and_user_information() {
        let url =
            Url::parse("https://user:secret@example.test/a?api_key=sentinel#fragment").unwrap();
        let rendered = DiagnosticEndpoint::from_url(&url).to_string();
        assert_eq!(rendered, "https://example.test/a");
        assert!(!rendered.contains("sentinel"));
        assert!(!rendered.contains("secret"));
    }

    #[test]
    fn public_address_classifier_rejects_special_use_and_embedded_private_addresses() {
        for address in [
            "0.1.2.3",
            "10.0.0.1",
            "100.64.0.1",
            "127.0.0.1",
            "169.254.1.1",
            "172.16.0.1",
            "192.0.2.1",
            "192.168.0.1",
            "198.18.0.1",
            "198.51.100.1",
            "203.0.113.1",
            "224.0.0.1",
            "240.0.0.1",
            "::1",
            "fc00::1",
            "fe80::1",
            "100::1",
            "2001:db8::1",
            "2001:db8:ffff:ffff::1",
            "::127.0.0.1",
            "::ffff:127.0.0.1",
            "64:ff9b::7f00:1",
            "2002:7f00:1::",
        ] {
            let address: IpAddr = address.parse().unwrap();
            assert!(!is_public_ip(address), "{address} must be blocked");
        }
        for address in ["1.1.1.1", "8.8.8.8", "2606:4700:4700::1111"] {
            let address: IpAddr = address.parse().unwrap();
            assert!(is_public_ip(address), "{address} must remain reachable");
        }
    }

    struct FixedResolver(Vec<SocketAddr>);

    #[async_trait]
    impl HostResolver for FixedResolver {
        async fn resolve(&self, _host: &str, _port: u16) -> io::Result<Vec<SocketAddr>> {
            Ok(self.0.clone())
        }
    }

    #[derive(Default)]
    struct ScriptedResolver {
        answers: Mutex<HashMap<String, VecDeque<Vec<SocketAddr>>>>,
    }

    impl ScriptedResolver {
        fn with_answers(answers: impl IntoIterator<Item = (String, Vec<Vec<SocketAddr>>)>) -> Self {
            Self {
                answers: Mutex::new(
                    answers
                        .into_iter()
                        .map(|(host, answers)| (host, answers.into()))
                        .collect(),
                ),
            }
        }
    }

    #[async_trait]
    impl HostResolver for ScriptedResolver {
        async fn resolve(&self, host: &str, _port: u16) -> io::Result<Vec<SocketAddr>> {
            let mut answers = self.answers.lock().expect("resolver answers");
            let sequence = answers.get_mut(host).ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, format!("no answer for {host}"))
            })?;
            match sequence.len() {
                0 => Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("no remaining answer for {host}"),
                )),
                1 => Ok(sequence.front().expect("one answer").clone()),
                _ => Ok(sequence.pop_front().expect("queued answer")),
            }
        }
    }

    fn public_address(port: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34)), port)
    }

    fn test_client(
        resolver: Arc<dyn HostResolver>,
        connector_address: SocketAddr,
    ) -> PublicHttpClient {
        let mut client = PublicHttpClient::with_resolver(resolver, PublicHttpPolicy::default());
        client.transport = SessionTransport::direct_for_tests();
        client.connector_addresses = Some(vec![connector_address]);
        client
    }

    #[tokio::test]
    async fn mixed_dns_answers_fail_before_connecting() {
        let client = PublicHttpClient::with_resolver(
            Arc::new(FixedResolver(vec![
                "1.1.1.1:443".parse().unwrap(),
                "127.0.0.1:443".parse().unwrap(),
            ])),
            PublicHttpPolicy::default(),
        );
        let error = client
            .get(
                PublicTarget::parse("https://example.test/path").unwrap(),
                HeaderMap::new(),
            )
            .await
            .expect_err("mixed public/private answers must fail closed");
        assert!(matches!(
            error,
            PublicHttpError::BlockedAddress {
                address: IpAddr::V4(address),
                ..
            } if address.is_loopback()
        ));
    }

    #[tokio::test]
    async fn redirect_to_private_literal_is_revalidated_and_rejected() {
        let server = MockServer::start().await;
        let port = server.address().port();
        Mock::given(method("GET"))
            .and(path("/start"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("location", format!("http://127.0.0.1:{port}/secret")),
            )
            .mount(&server)
            .await;
        let client = test_client(
            Arc::new(FixedResolver(vec![public_address(port)])),
            *server.address(),
        );
        let error = client
            .get(
                PublicTarget::parse(&format!("http://public.example:{port}/start")).unwrap(),
                HeaderMap::new(),
            )
            .await
            .expect_err("redirect targets must pass public-address validation");
        assert!(matches!(
            error,
            PublicHttpError::BlockedAddress {
                address: IpAddr::V4(address),
                ..
            } if address.is_loopback()
        ));
        assert_eq!(
            server.received_requests().await.expect("requests").len(),
            1,
            "the blocked redirect target must never be contacted"
        );
    }

    #[tokio::test]
    async fn same_host_redirect_is_resolved_again_and_rebinding_fails_closed() {
        let server = MockServer::start().await;
        let port = server.address().port();
        Mock::given(method("GET"))
            .and(path("/start"))
            .respond_with(ResponseTemplate::new(302).insert_header("location", "/next"))
            .mount(&server)
            .await;
        let resolver = ScriptedResolver::with_answers([(
            "rebind.example".to_owned(),
            vec![
                vec![public_address(port)],
                vec![SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)],
            ],
        )]);
        let client = test_client(Arc::new(resolver), *server.address());
        let error = client
            .get(
                PublicTarget::parse(&format!("http://rebind.example:{port}/start")).unwrap(),
                HeaderMap::new(),
            )
            .await
            .expect_err("every redirect hop must resolve and validate again");
        assert!(matches!(error, PublicHttpError::BlockedAddress { .. }));
        assert_eq!(server.received_requests().await.expect("requests").len(), 1);
    }

    #[tokio::test]
    async fn public_cross_origin_redirect_preserves_host_but_drops_credentials() {
        let server = MockServer::start().await;
        let port = server.address().port();
        Mock::given(method("GET"))
            .and(path("/start"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("location", format!("http://second.example:{port}/finish")),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/finish"))
            .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
            .mount(&server)
            .await;
        let resolver = ScriptedResolver::with_answers([
            ("first.example".to_owned(), vec![vec![public_address(port)]]),
            (
                "second.example".to_owned(),
                vec![vec![public_address(port)]],
            ),
        ]);
        let client = test_client(Arc::new(resolver), *server.address());
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, HeaderValue::from_static("Bearer sentinel"));
        headers.insert(COOKIE, HeaderValue::from_static("session=sentinel"));
        headers.insert(
            PROXY_AUTHORIZATION,
            HeaderValue::from_static("Basic sentinel"),
        );
        let response = client
            .get(
                PublicTarget::parse(&format!("http://first.example:{port}/start")).unwrap(),
                headers,
            )
            .await
            .expect("public cross-origin redirect");
        assert_eq!(response.status(), StatusCode::OK);
        let requests = server.received_requests().await.expect("requests");
        assert_eq!(requests.len(), 2);
        let finish = requests
            .iter()
            .find(|request| request.url.path() == "/finish")
            .expect("redirected request");
        let expected_host = format!("second.example:{port}");
        assert_eq!(
            finish
                .headers
                .get(reqwest::header::HOST)
                .and_then(|value| value.to_str().ok()),
            Some(expected_host.as_str())
        );
        assert!(!finish.headers.contains_key(AUTHORIZATION));
        assert!(!finish.headers.contains_key(COOKIE));
        assert!(!finish.headers.contains_key(PROXY_AUTHORIZATION));
    }

    #[tokio::test]
    async fn public_transport_uses_ambient_proxy_without_falling_back_direct() {
        let server = MockServer::start().await;
        let port = server.address().port();
        let executable = std::env::current_exe().expect("test executable");
        let output = tokio::task::spawn_blocking(move || {
            std::process::Command::new(executable)
                .arg("--exact")
                .arg("public_http::tests::direct_transport_proxy_child")
                .arg("--nocapture")
                .env("ZUNO_PUBLIC_HTTP_PROXY_CHILD", "1")
                .env("ZUNO_PUBLIC_HTTP_TEST_PORT", port.to_string())
                .env("HTTP_PROXY", "http://127.0.0.1:9")
                .env("HTTPS_PROXY", "http://127.0.0.1:9")
                .env("ALL_PROXY", "http://127.0.0.1:9")
                .env("http_proxy", "http://127.0.0.1:9")
                .env("https_proxy", "http://127.0.0.1:9")
                .env("all_proxy", "http://127.0.0.1:9")
                .env_remove("NO_PROXY")
                .env_remove("no_proxy")
                .output()
                .expect("proxy child process")
        })
        .await
        .expect("proxy child join");
        assert!(
            output.status.success(),
            "proxy child failed:\nstdout={}\nstderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            server
                .received_requests()
                .await
                .expect("origin requests")
                .is_empty(),
            "a failed selected proxy must never fall back to the origin"
        );
    }

    #[test]
    fn direct_transport_proxy_child() {
        if std::env::var_os("ZUNO_PUBLIC_HTTP_PROXY_CHILD").is_none() {
            return;
        }
        let port = std::env::var("ZUNO_PUBLIC_HTTP_TEST_PORT")
            .expect("test port")
            .parse::<u16>()
            .expect("numeric test port");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        runtime.block_on(async move {
            let client = PublicHttpClient::with_resolver(
                Arc::new(FixedResolver(vec![public_address(port)])),
                PublicHttpPolicy::default(),
            );
            let error = client
                .get(
                    PublicTarget::parse(&format!("http://public.example:{port}/direct")).unwrap(),
                    HeaderMap::new(),
                )
                .await
                .expect_err("the selected proxy is unreachable");
            assert!(matches!(
                error,
                PublicHttpError::Transport {
                    route: "http_proxy",
                    ..
                }
            ));
        });
    }

    struct Nat64Resolver;

    #[async_trait]
    impl HostResolver for Nat64Resolver {
        async fn resolve(&self, host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
            let address = if host == IPV4ONLY_ARPA {
                "2001:4860:64:64::c000:aa"
            } else {
                "2001:4860:64:64::7f00:1"
            };
            Ok(vec![SocketAddr::new(address.parse().unwrap(), port)])
        }
    }

    #[tokio::test]
    async fn discovered_nat64_prefix_rejects_embedded_private_destination() {
        let client =
            PublicHttpClient::with_resolver(Arc::new(Nat64Resolver), PublicHttpPolicy::default());
        let error = client
            .get(
                PublicTarget::parse("https://nat64.test/path").unwrap(),
                HeaderMap::new(),
            )
            .await
            .expect_err("NAT64 private destination must fail before connecting");
        assert!(matches!(
            error,
            PublicHttpError::BlockedNat64Address {
                translated,
                ..
            } if translated.is_loopback()
        ));
    }

    #[test]
    fn rfc_6052_prefix_extraction_supports_all_layouts() {
        for (prefix_length, address) in [
            (32, "2001:db8:c000:00aa::"),
            (40, "2001:db8:01c0:0000:00aa::"),
            (48, "2001:db8:0001:c000:0000:aa00::"),
            (56, "2001:db8:0001:02c0:0000:00aa::"),
            (64, "2001:db8:0001:0002:00c0:0000:aa00:0"),
            (96, "2001:db8:0001:0002::c000:00aa"),
        ] {
            let bytes = address.parse::<Ipv6Addr>().unwrap().octets();
            assert_eq!(
                embedded_ipv4(&bytes, prefix_length),
                Some(Ipv4Addr::new(192, 0, 0, 170)),
                "prefix length {prefix_length}"
            );
        }
    }
}
