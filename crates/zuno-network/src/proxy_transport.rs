//! HTTP/1 transport that keeps public-target validation authoritative when a proxy is used.
//!
//! Reqwest's ordinary proxy connector sends the origin hostname in an HTTP CONNECT
//! request. That is correct for a browser, but it lets the proxy perform a second DNS
//! lookup after Zuno validated a different answer. This transport instead sends one of
//! the already-validated IP addresses to HTTP, HTTPS, SOCKS4, and SOCKS5 proxies, while
//! retaining the original hostname in the HTTP `Host` header and TLS SNI.

use bytes::Bytes;
use http::header::{HOST, PROXY_AUTHORIZATION};
use http::{HeaderMap, HeaderValue, Method, Request, Response, StatusCode, Uri};
use http_body_util::Empty;
use hyper::body::Incoming;
use hyper::client::conn::http1;
use hyper_util::client::proxy::matcher::Matcher;
use hyper_util::rt::TokioIo;
use rustls::ClientConfig;
use rustls::pki_types::ServerName;
use rustls_platform_verifier::ConfigVerifierExt as _;
use std::env;
use std::fmt;
use std::future::Future;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use url::Url;

use crate::public_http::PublicTarget;

trait AsyncIo: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T> AsyncIo for T where T: AsyncRead + AsyncWrite + Unpin + Send {}
type BoxIo = Box<dyn AsyncIo>;

/// Credential-free route identity suitable for logs and timeout reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RouteKind {
    Direct,
    NoProxy,
    HttpProxy,
    HttpsProxy,
    Socks4Proxy,
    Socks5Proxy,
}

impl RouteKind {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::NoProxy => "no_proxy",
            Self::HttpProxy => "http_proxy",
            Self::HttpsProxy => "https_proxy",
            Self::Socks4Proxy => "socks4_proxy",
            Self::Socks5Proxy => "socks5_proxy",
        }
    }
}

/// Per-address connection-establishment budget.
///
/// A blackholed address otherwise spends the caller's whole request deadline inside the
/// kernel SYN retry sequence, and an address that answers the SYN and then goes silent
/// spends it inside the TLS handshake, the CONNECT round trip, or a SOCKS reply, so a
/// second, reachable validated address never gets an attempt at all. This budget
/// therefore covers one whole attempt - TCP, proxy negotiation, and the TLS handshake -
/// and deliberately stops before the request: waiting for response headers is the
/// caller's deadline to own, and bounding it here would abandon a slow but healthy page.
///
/// The default is a fixed per-attempt value on purpose. Dividing a total budget by the
/// number of resolved addresses would let a large DNS answer - which the target's own
/// zone controls - shrink every attempt below a legitimate handshake and fail an origin
/// that is merely far away. A caller whose own request budget is smaller than this
/// supplies its own value through [`PublicHttpClient::with_establish_timeout`], which
/// clamps into `[ESTABLISH_TIMEOUT_FLOOR, ESTABLISH_ATTEMPT_TIMEOUT]`: a caller can only
/// tighten the window a single stalled address holds, never widen it - and, through
/// [`total_establish_budget`], the whole sequence with it.
///
/// [`PublicHttpClient::with_establish_timeout`]: crate::PublicHttpClient::with_establish_timeout
const ESTABLISH_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(10);

/// The smallest per-attempt budget a caller can ask for.
///
/// A zero or near-zero budget would fail every connection before a packet moved, so a
/// caller asking for less than this gets the floor rather than a transport that can never
/// succeed. The floor is deliberately far below any wide-area handshake: it exists to
/// reject the degenerate value, not to second-guess a caller that wants a tight bound.
const ESTABLISH_TIMEOUT_FLOOR: Duration = Duration::from_millis(100);

/// Ceiling on one whole establishment sequence, however many addresses it walks.
///
/// The per-address budget alone bounds one attempt; the number of attempts is the size of
/// a DNS answer, which the destination's own zone chooses. Without this ceiling a peer
/// that answers with 30 unroutable addresses holds one request for 30 x the per-address
/// budget, so the fix for a single silent address would have turned the total cost into a
/// function of a peer-supplied count. This is a constant rather than a derived value: no
/// caller and no answer can raise it, and it matches the connect ceiling every reqwest
/// client in this crate already carries (`crate::CONNECT_TIMEOUT`).
const ESTABLISH_TOTAL_TIMEOUT: Duration = Duration::from_secs(30);

/// How many stalled addresses one sequence may pay for before it gives up.
///
/// The total budget is this multiple of the per-address budget, capped by
/// [`ESTABLISH_TOTAL_TIMEOUT`]. Deriving the total from the per-address value rather than
/// the reverse is deliberate: dividing a fixed total by the number of resolved addresses
/// would let a large answer shrink every attempt below a legitimate handshake, which is
/// the failure the per-address budget exists to avoid.
const ESTABLISH_STALLED_ADDRESS_ALLOWANCE: u32 = 3;

/// Ceiling on how many addresses one establishment sequence walks.
///
/// The time ceiling bounds stalled addresses, but an address that answers instantly costs
/// almost nothing, so a large answer of denied or refused addresses turns one request into
/// one proxy round trip per record. `interleave_families` alternates families, so this
/// cap still reaches both an A and a AAAA record long before it is spent.
const MAX_ESTABLISH_ATTEMPTS: usize = 8;

/// The whole-sequence budget implied by a per-address budget.
fn total_establish_budget(attempt_timeout: Duration) -> Duration {
    attempt_timeout
        .saturating_mul(ESTABLISH_STALLED_ADDRESS_ALLOWANCE)
        .min(ESTABLISH_TOTAL_TIMEOUT)
}

/// A transport failure split by whether repeating the request can change the answer.
///
/// Trust, credential, and protocol rejections are decisions rather than weather: the peer
/// answers them identically on the next attempt, so retrying them on backoff only spends
/// the caller's budget while hiding a misconfiguration - or an active interception -
/// behind a delay.
#[derive(Debug)]
pub(crate) enum TransportFailure {
    /// Socket, DNS, and HTTP framing failures that a later attempt may survive.
    Transient(io::Error),
    /// A refusal that names one destination address rather than the request.
    ///
    /// A proxy ACL can permit one validated address for an origin and deny another, so
    /// this answer is final for the address that produced it and must not cancel the
    /// remaining validated addresses - the walk continues. It is still a decision the peer
    /// made, so it decides the request when no address connected: see `establish_in_turn`
    /// for why another address's weather is not allowed to speak for a denied one.
    PermanentForAddress(io::Error),
    /// Certificate, credential, and protocol rejections that cannot succeed later.
    Permanent(io::Error),
}

impl TransportFailure {
    /// Whether repeating the request can change this answer.
    pub(crate) const fn is_permanent(&self) -> bool {
        matches!(self, Self::Permanent(_) | Self::PermanentForAddress(_))
    }

    /// Whether the remaining validated addresses cannot change this answer either.
    const fn short_circuits(&self) -> bool {
        matches!(self, Self::Permanent(_))
    }

    pub(crate) fn into_io(self) -> io::Error {
        match self {
            Self::Transient(error) | Self::PermanentForAddress(error) | Self::Permanent(error) => {
                error
            }
        }
    }

    /// A socket, resolver, or framing failure: the classification the peer did not decide.
    fn transient(error: io::Error) -> Self {
        Self::Transient(error)
    }

    fn invalid_input(message: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> Self {
        Self::Permanent(io::Error::new(io::ErrorKind::InvalidInput, message))
    }
}

#[derive(Clone)]
pub(crate) struct SessionTransport {
    proxies: Arc<ProxyEnvironment>,
    tls: Arc<ClientConfig>,
    establish_timeout: Duration,
}

impl fmt::Debug for SessionTransport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionTransport")
            .field("proxies", &self.proxies)
            .field("establish_timeout", &self.establish_timeout)
            .finish_non_exhaustive()
    }
}

/// The process-wide TLS configuration for public HTTP.
///
/// `with_platform_verifier` enumerates and parses the platform trust store with blocking
/// filesystem work and memoizes nothing, while `zuno acp` and `zuno serve` construct
/// clients on a current-thread runtime whose only thread also delivers streaming output.
/// The trust material is *treated* as immutable for the life of the process, so it is
/// parsed once.
///
/// That is a choice, not a fact about the platform. rustls-platform-verifier keeps its
/// certificates per verifier precisely so that rebuilding one re-reads the roots from disk,
/// and before this memoization each client did. The operational consequence is explicit: a
/// root removed or added after the first public request - a revoked corporate CA, a
/// platform CA update - does not take effect in a long-running `zuno serve` or `zuno acp`
/// process until it restarts. A deployment that rotates trust anchors without restarting
/// needs the restart, and a future change that must pick roots up live has to give each
/// client its own `ClientConfig` and accept the per-client parse.
///
/// Sharing one `ClientConfig` also shares its client session store, so TLS resumption
/// tickets are now process-wide rather than per-client: two `PublicHttpClient` values that
/// reach the same origin can resume each other's session. That is intended - it is what
/// makes a second request to the same origin cheap - but it means the store is a
/// cross-caller surface. rustls' default store is bounded (256 servers, in memory only,
/// never written to disk), and every caller here already shares one process trust store
/// and one proxy environment, so no caller learns anything it could not reach directly.
/// A caller that must not share resumption state needs its own `ClientConfig`, not a
/// second `PublicHttpClient`.
fn shared_tls_config() -> Arc<ClientConfig> {
    static TLS: OnceLock<Arc<ClientConfig>> = OnceLock::new();
    Arc::clone(TLS.get_or_init(|| {
        let mut tls = ClientConfig::with_platform_verifier()
            .expect("platform TLS verifier must initialize for public HTTP");
        tls.alpn_protocols = vec![b"http/1.1".to_vec()];
        Arc::new(tls)
    }))
}

impl SessionTransport {
    pub(crate) fn from_process() -> Self {
        Self {
            proxies: Arc::new(ProxyEnvironment::from_process()),
            tls: shared_tls_config(),
            establish_timeout: ESTABLISH_ATTEMPT_TIMEOUT,
        }
    }

    /// Tighten the per-address establishment budget for one client.
    ///
    /// The value is clamped into `[ESTABLISH_TIMEOUT_FLOOR, ESTABLISH_ATTEMPT_TIMEOUT]`.
    /// The ceiling is the load-bearing half: no caller can widen how long one stalled
    /// address holds the request, so a caller that supplies an attacker-influenced number
    /// - a config file, a tool argument - can only make the transport give up sooner.
    pub(crate) fn set_establish_timeout(&mut self, per_address: Duration) {
        self.establish_timeout =
            per_address.clamp(ESTABLISH_TIMEOUT_FLOOR, ESTABLISH_ATTEMPT_TIMEOUT);
    }

    /// Set the budget a test needs without the public floor.
    #[cfg(test)]
    pub(crate) fn set_establish_timeout_for_tests(&mut self, per_address: Duration) {
        self.establish_timeout = per_address;
    }

    #[cfg(test)]
    pub(crate) fn direct_for_tests() -> Self {
        let mut transport = Self::from_process();
        transport.proxies = Arc::new(ProxyEnvironment {
            http: None,
            https: None,
            all: None,
            no_proxy: String::new(),
        });
        transport
    }

    pub(crate) fn route_label(&self, target: &PublicTarget) -> Result<RouteKind, io::Error> {
        Ok(self.proxies.route(target)?.kind())
    }

    pub(crate) async fn get(
        &self,
        target: &PublicTarget,
        headers: HeaderMap,
        validated_addresses: &[SocketAddr],
        direct_addresses: &[SocketAddr],
    ) -> Result<(Response<Incoming>, RouteKind), TransportFailure> {
        let route = self
            .proxies
            .route(target)
            .map_err(TransportFailure::Permanent)?;
        let kind = route.kind();
        // Each arm establishes one whole connection - TCP, proxy negotiation, and the
        // origin TLS handshake - inside the per-address attempt, so a proxy that opens a
        // tunnel and then relays nothing yields to the next validated address instead of
        // consuming the caller's entire request.
        let response = match route {
            Route::Direct { .. } => {
                let stream =
                    establish_direct(target, direct_addresses, &self.tls, self.establish_timeout)
                        .await?;
                send_origin(stream, target, headers).await?
            }
            Route::HttpProxy { proxy, auth } => {
                if target.url().scheme() == "https" {
                    let stream = establish_tunnel(
                        &proxy,
                        target,
                        &self.tls,
                        validated_addresses,
                        auth.as_ref(),
                        self.establish_timeout,
                    )
                    .await?;
                    send_origin(stream, target, headers).await?
                } else {
                    // A forward-proxied plaintext request carries the validated address in
                    // the request line, so retrying another address would be a second
                    // request rather than a second connection attempt; the caller's retry
                    // owns that.
                    let validated = interleave_families(validated_addresses)
                        .first()
                        .copied()
                        .ok_or_else(|| {
                            TransportFailure::transient(io::Error::new(
                                io::ErrorKind::AddrNotAvailable,
                                "no validated target address",
                            ))
                        })?;
                    // This route has no address loop of its own, so the sequence ceiling
                    // has to be applied here: `connect_http_proxy` resolves the proxy
                    // hostname and then walks the proxy's own answer.
                    let stream = establish_within_total(self.establish_timeout, |attempt| {
                        connect_http_proxy(&proxy, Arc::clone(&self.tls), attempt)
                    })
                    .await?;
                    send_forward_proxy(stream, target, headers, validated, auth.as_ref()).await?
                }
            }
            Route::Socks4Proxy { proxy, username } => {
                let stream = establish_socks4(
                    &proxy,
                    target,
                    &self.tls,
                    validated_addresses,
                    &username,
                    self.establish_timeout,
                )
                .await?;
                send_origin(stream, target, headers).await?
            }
            Route::Socks5Proxy { proxy, credentials } => {
                let stream = establish_socks5(
                    &proxy,
                    target,
                    &self.tls,
                    validated_addresses,
                    credentials.as_ref(),
                    self.establish_timeout,
                )
                .await?;
                send_origin(stream, target, headers).await?
            }
        };
        Ok((response, kind))
    }
}

#[derive(Clone)]
struct ProxyEnvironment {
    http: Option<String>,
    https: Option<String>,
    all: Option<String>,
    no_proxy: String,
}

impl fmt::Debug for ProxyEnvironment {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProxyEnvironment")
            .field("http", &self.http.as_ref().map(|_| "<configured>"))
            .field("https", &self.https.as_ref().map(|_| "<configured>"))
            .field("all", &self.all.as_ref().map(|_| "<configured>"))
            .field("no_proxy", &(!self.no_proxy.is_empty()))
            .finish()
    }
}

impl ProxyEnvironment {
    fn from_process() -> Self {
        // A CGI-style host maps an inbound request's `Proxy:` header onto the child's
        // `HTTP_PROXY` variable (httpoxy). Hyper-util drops the proxy environment when
        // `REQUEST_METHOD` is present, so reqwest clients built here already ignore the
        // poisoned value; this hand-rolled resolution must not be the one path that
        // trusts it.
        if env::var_os("REQUEST_METHOD").is_some() {
            return Self {
                http: None,
                https: None,
                all: None,
                no_proxy: String::new(),
            };
        }
        Self {
            http: first_env("HTTP_PROXY", "http_proxy"),
            https: first_env("HTTPS_PROXY", "https_proxy"),
            all: first_env("ALL_PROXY", "all_proxy"),
            no_proxy: first_env("NO_PROXY", "no_proxy").unwrap_or_default(),
        }
    }

    fn route(&self, target: &PublicTarget) -> Result<Route, io::Error> {
        let selected = match target.url().scheme() {
            "http" => self.http.as_ref().or(self.all.as_ref()),
            "https" => self.https.as_ref().or(self.all.as_ref()),
            _ => None,
        };
        let Some(selected) = selected else {
            return Ok(Route::Direct { no_proxy: false });
        };

        let target_uri = target_uri(target)?;
        let parsed = Matcher::builder()
            .all(selected.as_str())
            .build()
            .intercept(&target_uri)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "configured proxy URL has an unsupported or malformed scheme",
                )
            })?;

        if !self.no_proxy.is_empty()
            && Matcher::builder()
                .all(selected.as_str())
                .no(self.no_proxy.as_str())
                .build()
                .intercept(&target_uri)
                .is_none()
        {
            return Ok(Route::Direct { no_proxy: true });
        }

        let proxy = parsed.uri().clone();
        match proxy.scheme_str() {
            Some("http" | "https") => Ok(Route::HttpProxy {
                proxy,
                auth: parsed.basic_auth().cloned(),
            }),
            Some("socks4" | "socks4a") => Ok(Route::Socks4Proxy {
                proxy,
                username: parsed
                    .raw_auth()
                    .map(|(username, _)| username.to_owned())
                    .unwrap_or_default(),
            }),
            Some("socks5" | "socks5h") => Ok(Route::Socks5Proxy {
                proxy,
                credentials: parsed
                    .raw_auth()
                    .map(|(username, password)| (username.to_owned(), password.to_owned())),
            }),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "configured proxy URL has an unsupported scheme",
            )),
        }
    }
}

enum Route {
    Direct {
        no_proxy: bool,
    },
    HttpProxy {
        proxy: Uri,
        auth: Option<HeaderValue>,
    },
    Socks4Proxy {
        proxy: Uri,
        username: String,
    },
    Socks5Proxy {
        proxy: Uri,
        credentials: Option<(String, String)>,
    },
}

impl Route {
    fn kind(&self) -> RouteKind {
        match self {
            Self::Direct { no_proxy: true } => RouteKind::NoProxy,
            Self::Direct { no_proxy: false } => RouteKind::Direct,
            Self::HttpProxy { proxy, .. } if proxy.scheme() == Some(&http::uri::Scheme::HTTPS) => {
                RouteKind::HttpsProxy
            }
            Self::HttpProxy { .. } => RouteKind::HttpProxy,
            Self::Socks4Proxy { .. } => RouteKind::Socks4Proxy,
            Self::Socks5Proxy { .. } => RouteKind::Socks5Proxy,
        }
    }
}

fn first_env(upper: &str, lower: &str) -> Option<String> {
    env::var(upper)
        .ok()
        .or_else(|| env::var(lower).ok())
        .filter(|value| !value.trim().is_empty())
}

fn target_uri(target: &PublicTarget) -> Result<Uri, io::Error> {
    target
        .url()
        .as_str()
        .parse()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))
}

fn target_host(target: &PublicTarget) -> Result<&str, io::Error> {
    target
        .url()
        .host_str()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "target has no host"))
}

fn origin_authority(target: &PublicTarget) -> Result<String, io::Error> {
    // `Url::host_str` already brackets an IPv6 literal, which is the form the `Host`
    // header and a CONNECT authority want.
    let host = target_host(target)?;
    Ok(match target.url().port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    })
}

/// A host name as a resolver and a TLS verifier accept it.
///
/// URL and URI renderings keep the brackets around an IPv6 literal. `lookup_host` and
/// `ServerName` both reject the bracketed form, so an IPv6-literal target or proxy would
/// fail before a single packet moved.
fn unbracketed(host: &str) -> &str {
    host.strip_prefix('[')
        .and_then(|inner| inner.strip_suffix(']'))
        .unwrap_or(host)
}

/// The TLS name for an origin target.
fn tls_server_name(target: &PublicTarget) -> Result<ServerName<'static>, TransportFailure> {
    server_name(target_host(target).map_err(TransportFailure::Permanent)?)
}

fn server_name(host: &str) -> Result<ServerName<'static>, TransportFailure> {
    ServerName::try_from(unbracketed(host).to_owned()).map_err(|error| {
        TransportFailure::invalid_input(format!("invalid TLS server name: {error}"))
    })
}

fn path_and_query(url: &Url) -> &str {
    let path = &url[url::Position::BeforePath..url::Position::AfterQuery];
    if path.is_empty() { "/" } else { path }
}

fn request_headers(
    target: &PublicTarget,
    mut headers: HeaderMap,
) -> Result<HeaderMap, TransportFailure> {
    headers.remove(PROXY_AUTHORIZATION);
    let authority = origin_authority(target).map_err(TransportFailure::Permanent)?;
    headers.insert(
        HOST,
        HeaderValue::from_str(&authority).map_err(|error| {
            // A host that cannot be written as a header is a property of the URL, so no
            // later attempt renders it differently.
            TransportFailure::invalid_input(format!(
                "target authority is not a Host value: {error}"
            ))
        })?,
    );
    Ok(headers)
}

/// Establish a direct connection to one validated address, TLS included.
async fn establish_direct(
    target: &PublicTarget,
    addresses: &[SocketAddr],
    tls: &Arc<ClientConfig>,
    attempt_timeout: Duration,
) -> Result<BoxIo, TransportFailure> {
    establish_in_turn(addresses, attempt_timeout, |address| {
        let tls = Arc::clone(tls);
        async move {
            let stream: BoxIo = Box::new(
                TcpStream::connect(address)
                    .await
                    .map_err(TransportFailure::transient)?,
            );
            secure_origin_if_needed(stream, target, tls, attempt_timeout).await
        }
    })
    .await
}

async fn connect_any(
    addresses: &[SocketAddr],
    attempt_timeout: Duration,
) -> Result<BoxIo, TransportFailure> {
    establish_in_turn(addresses, attempt_timeout, |address| async move {
        Ok(Box::new(
            TcpStream::connect(address)
                .await
                .map_err(TransportFailure::transient)?,
        ) as BoxIo)
    })
    .await
}

/// Try each address in turn, bounding the sequence as well as every attempt.
///
/// The attempt is injected so the timeouts, the family ordering, and the classification
/// rules can be exercised without a blackholed address on the host running the tests, and
/// so every route - direct, CONNECT tunnel, SOCKS4, SOCKS5 - shares one implementation of
/// those rules instead of four copies.
///
/// Two peer-supplied quantities are bounded here. The per-address budget bounds one silent
/// address; [`total_establish_budget`] and [`MAX_ESTABLISH_ATTEMPTS`] bound the *number* of
/// addresses, which is the size of a DNS answer the destination's own zone chooses.
///
/// The aggregate classification is the second thing this function owns. A
/// [`TransportFailure::PermanentForAddress`] answer is a decision the peer made about one
/// destination; every other outcome in the sequence is either our own budget expiring or a
/// failure at a *different* address. Neither is evidence that the denied destination will
/// answer differently later, so a denial decides the request even when another address only
/// had weather. Letting the aggregate widen instead would put the retry decision in the
/// hands of whoever controls the DNS answer: adding one unroutable A record would make any
/// proxy-side denial retryable on every backoff step.
async fn establish_in_turn<F, Fut>(
    addresses: &[SocketAddr],
    attempt_timeout: Duration,
    attempt: F,
) -> Result<BoxIo, TransportFailure>
where
    F: Fn(SocketAddr) -> Fut,
    Fut: Future<Output = Result<BoxIo, TransportFailure>>,
{
    let ordered = interleave_families(addresses);
    let total = total_establish_budget(attempt_timeout);
    let started = Instant::now();
    // The smallest attempt worth starting: below this the attempt can only report our own
    // deadline, which is a wasted socket rather than a diagnosis.
    let smallest_useful = attempt_timeout.min(ESTABLISH_TIMEOUT_FLOOR);
    let mut denied: Option<TransportFailure> = None;
    let mut last: Option<TransportFailure> = None;
    let mut abandoned: Option<String> = None;
    // The index is the number of attempts already made: every iteration that does not break
    // performs exactly one attempt.
    for (attempts, address) in ordered.iter().enumerate() {
        if attempts >= MAX_ESTABLISH_ATTEMPTS {
            abandoned = Some(format!(
                "gave up after {attempts} of {} validated addresses",
                ordered.len()
            ));
            break;
        }
        let remaining = total.saturating_sub(started.elapsed());
        if remaining < smallest_useful {
            abandoned = Some(format!(
                "gave up after {attempts} of {} validated addresses spent the {total:?} \
                 establishment budget",
                ordered.len()
            ));
            break;
        }
        let budget = attempt_timeout.min(remaining);
        let failure = match tokio::time::timeout(budget, attempt(*address)).await {
            Ok(Ok(stream)) => return Ok(stream),
            // A rejected credential, certificate, or protocol is decided before the
            // destination address matters, so the remaining addresses meet the same
            // refusal and opening more sockets to them buys nothing.
            Ok(Err(failure)) if failure.short_circuits() => return Err(failure),
            Ok(Err(failure)) => failure,
            Err(_elapsed) => TransportFailure::transient(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("establishing a connection to {address} exceeded {budget:?}"),
            )),
        };
        match failure {
            // Keep the first denial: with a truncated walk it is the one answer we are
            // certain we received, and it names the address the peer actually refused.
            TransportFailure::PermanentForAddress(_) if denied.is_none() => {
                denied = Some(failure);
            }
            failure => last = Some(failure),
        }
    }
    // A destination-scoped denial outranks everything else the sequence saw. Addresses that
    // stalled, that we skipped, or that failed for their own reasons are not evidence that
    // this destination stops being denied.
    if let Some(denied) = denied {
        return Err(denied);
    }
    Err(match (abandoned, last) {
        // Our own ceiling stopped the walk. That is not a peer decision, so the request
        // stays retryable - but say so, rather than reporting one address's timeout as if
        // the whole answer set had been tried.
        (Some(reason), Some(last)) => TransportFailure::transient(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("{reason}: {}", last.into_io()),
        )),
        (None, Some(last)) => last,
        // An empty address set is not a peer decision: `PublicHttpClient` rejects an empty
        // answer set before the transport sees it, and a resolver that answers with no
        // records can answer differently later. Reporting this as permanent would turn
        // "matched nothing" into a hard failure.
        (_, None) => TransportFailure::transient(io::Error::new(
            io::ErrorKind::AddrNotAvailable,
            "no address to connect",
        )),
    })
}

/// Bound one establishment that has no address loop of its own.
///
/// The forward-proxy route sends the validated address in the request line rather than
/// opening a tunnel per address, so it never enters `establish_in_turn` - but it still
/// resolves the proxy hostname and walks whatever that answer contains, so it needs the
/// same sequence ceiling.
async fn establish_within_total<F, Fut>(
    attempt_timeout: Duration,
    establish: F,
) -> Result<BoxIo, TransportFailure>
where
    F: FnOnce(Duration) -> Fut,
    Fut: Future<Output = Result<BoxIo, TransportFailure>>,
{
    let total = total_establish_budget(attempt_timeout);
    match tokio::time::timeout(total, establish(attempt_timeout)).await {
        Ok(result) => result,
        Err(_elapsed) => Err(TransportFailure::transient(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("establishing a proxy connection exceeded {total:?}"),
        ))),
    }
}

/// Alternate address families, keeping the resolver's preference first.
///
/// A host whose IPv6 egress is blackholed otherwise burns one full attempt budget per
/// AAAA record before the first reachable A record is tried.
fn interleave_families(addresses: &[SocketAddr]) -> Vec<SocketAddr> {
    let Some(first) = addresses.first() else {
        return Vec::new();
    };
    let preferred_is_ipv6 = first.is_ipv6();
    let (preferred, other): (Vec<_>, Vec<_>) = addresses
        .iter()
        .copied()
        .partition(|address| address.is_ipv6() == preferred_is_ipv6);
    let mut ordered = Vec::with_capacity(addresses.len());
    let mut preferred = preferred.into_iter();
    let mut other = other.into_iter();
    loop {
        let taken = ordered.len();
        ordered.extend(preferred.next());
        ordered.extend(other.next());
        if ordered.len() == taken {
            return ordered;
        }
    }
}

/// Resolve a proxy hostname and connect to one of its addresses.
///
/// The resolution carries the per-address budget of its own: it is awaited outside any
/// attempt loop on the forward-proxy route, so a proxy whose DNS answer never arrives
/// would otherwise hold the request with no deadline at all.
async fn connect_host(
    host: &str,
    port: u16,
    attempt_timeout: Duration,
) -> Result<BoxIo, TransportFailure> {
    let lookup = tokio::net::lookup_host((unbracketed(host), port));
    let addresses = match tokio::time::timeout(attempt_timeout, lookup).await {
        Ok(resolved) => resolved.map_err(TransportFailure::transient)?,
        Err(_elapsed) => {
            return Err(TransportFailure::transient(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("resolving the proxy host exceeded {attempt_timeout:?}"),
            )));
        }
    };
    let addresses = addresses.collect::<Vec<_>>();
    connect_any(&addresses, attempt_timeout).await
}

async fn tls_connect(
    stream: BoxIo,
    server_name: ServerName<'static>,
    config: Arc<ClientConfig>,
    attempt_timeout: Duration,
) -> Result<BoxIo, TransportFailure> {
    // tokio-rustls already reports an `io::Error`; wrapping it again would bury the typed
    // rustls error that decides whether this failure is worth retrying. The handshake
    // carries its own budget as well as the per-address one, because the handshake to an
    // HTTPS proxy runs outside any address loop.
    let handshake = TlsConnector::from(config).connect(server_name, stream);
    match tokio::time::timeout(attempt_timeout, handshake).await {
        Ok(Ok(stream)) => Ok(Box::new(stream)),
        Ok(Err(error)) => Err(classify_tls_error(error)),
        Err(_elapsed) => Err(TransportFailure::transient(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("TLS handshake exceeded {attempt_timeout:?}"),
        ))),
    }
}

/// Classify a handshake failure on the typed rustls error, never on its rendering.
///
/// tokio-rustls reports a rustls rejection as `io::Error(InvalidData, rustls::Error)` and
/// passes an underlying socket failure through unchanged, so a reset mid-handshake stays
/// retryable while a rejected certificate does not.
fn classify_tls_error(error: io::Error) -> TransportFailure {
    let permanent = error
        .get_ref()
        .and_then(|source| source.downcast_ref::<rustls::Error>())
        .is_some_and(is_permanent_tls_error);
    if permanent {
        TransportFailure::Permanent(error)
    } else {
        TransportFailure::Transient(error)
    }
}

/// Whether a rustls rejection is an answer rather than weather.
///
/// The whole enum is listed rather than the handful of variants that motivated this
/// classification, because a protocol-level rejection is exactly the shape a captive
/// portal or an interception box produces: plaintext on 443 deframes as
/// `InvalidMessage(InvalidContentType)`, and retrying that on backoff hides the
/// interception behind a delay. `rustls::Error` is `#[non_exhaustive]`, so a match without
/// a wildcard cannot compile here; the wildcard therefore reports permanent, which keeps a
/// variant added by a future rustls upgrade out of the retry path instead of silently
/// reopening this hole.
///
/// The compiler is not a guard for that upgrade. A `#[non_exhaustive]` enum absorbs a new
/// variant into the wildcard with no error and no warning, so a rustls bump requires
/// re-auditing this function by hand: a newly added variant that is genuinely transient
/// becomes a hard failure for every public fetch until it is listed above. The direction is
/// the safe one - a missed variant stops retrying rather than retrying an answer - but it is
/// not free, and nothing mechanical will point at it.
fn is_permanent_tls_error(error: &rustls::Error) -> bool {
    use rustls::Error as Tls;
    match error {
        // A fatal alert is only weather when the peer says the fault was its own or that
        // it went away: a load balancer draining a node, an overloaded server answering
        // internal_error, or an operator-cancelled handshake.
        Tls::AlertReceived(alert) => !is_transient_alert(*alert),
        // Trust decisions, except the ones that only report that revocation state could
        // not be reached.
        Tls::InvalidCertificate(certificate) => !is_transient_certificate_error(certificate),
        // Local resource starvation: the clock or the system RNG was momentarily
        // unavailable, which the next attempt can survive.
        Tls::FailedToGetCurrentTime | Tls::FailedToGetRandomBytes => false,
        // Protocol-level rejections and local defects. Every one of these is decided by
        // what was on the wire or by this process's own configuration, so the next attempt
        // reproduces it: a plaintext or intercepted response, a peer that deviates from
        // TLS, a chain this verifier cannot process, or a parameter this build rejects.
        Tls::InappropriateMessage { .. }
        | Tls::InappropriateHandshakeMessage { .. }
        | Tls::InvalidEncryptedClientHello(_)
        | Tls::InvalidMessage(_)
        | Tls::NoCertificatesPresented
        | Tls::UnsupportedNameType
        | Tls::DecryptError
        | Tls::EncryptError
        | Tls::PeerIncompatible(_)
        | Tls::PeerMisbehaved(_)
        | Tls::InvalidCertRevocationList(_)
        | Tls::General(_)
        | Tls::HandshakeNotComplete
        | Tls::PeerSentOversizedRecord
        | Tls::NoApplicationProtocol
        | Tls::BadMaxFragmentSize
        | Tls::InconsistentKeys(_)
        | Tls::Other(_) => true,
        // A variant this build of rustls does not have. Reported permanent so a dependency
        // bump cannot silently re-open the retry path; see the note above about the manual
        // re-audit a bump requires.
        _ => true,
    }
}

/// Alerts that describe the peer's own state rather than a refusal of this request.
fn is_transient_alert(alert: rustls::AlertDescription) -> bool {
    matches!(
        alert,
        rustls::AlertDescription::CloseNotify
            | rustls::AlertDescription::InternalError
            | rustls::AlertDescription::UserCanceled
    )
}

/// Certificate outcomes that report unreachable revocation state, not a bad certificate.
///
/// rustls-platform-verifier funnels every platform trust error it does not map into
/// `CertificateError::Other`, and that bucket contains responder-reachability failures -
/// `CRYPT_E_REVOCATION_OFFLINE` on Windows, the `errSecOCSP*` family on macOS - alongside
/// genuine rejections. Since the payload can only be told apart by its rendered text, and
/// retry decisions here are typed, `Other` stays retryable: a macOS or Windows user whose
/// revocation responder is briefly unreachable keeps the retry that used to succeed, at
/// the cost of retrying an unmapped hard rejection on those two platforms.
fn is_transient_certificate_error(error: &rustls::CertificateError) -> bool {
    matches!(
        error,
        rustls::CertificateError::UnknownRevocationStatus
            | rustls::CertificateError::ExpiredRevocationList
            | rustls::CertificateError::ExpiredRevocationListContext { .. }
            | rustls::CertificateError::Other(_)
    )
}

async fn secure_origin_if_needed(
    stream: BoxIo,
    target: &PublicTarget,
    config: Arc<ClientConfig>,
    attempt_timeout: Duration,
) -> Result<BoxIo, TransportFailure> {
    if target.url().scheme() == "https" {
        tls_connect(stream, tls_server_name(target)?, config, attempt_timeout).await
    } else {
        Ok(stream)
    }
}

async fn connect_http_proxy(
    proxy: &Uri,
    tls: Arc<ClientConfig>,
    attempt_timeout: Duration,
) -> Result<BoxIo, TransportFailure> {
    let host = proxy
        .host()
        .ok_or_else(|| TransportFailure::invalid_input("proxy has no host"))?;
    let port = proxy.port_u16().unwrap_or_else(|| {
        if proxy.scheme() == Some(&http::uri::Scheme::HTTPS) {
            443
        } else {
            80
        }
    });
    let stream = connect_host(host, port, attempt_timeout).await?;
    if proxy.scheme() == Some(&http::uri::Scheme::HTTPS) {
        tls_connect(stream, server_name(host)?, tls, attempt_timeout).await
    } else {
        Ok(stream)
    }
}

async fn send_origin(
    stream: BoxIo,
    target: &PublicTarget,
    headers: HeaderMap,
) -> Result<Response<Incoming>, TransportFailure> {
    let request = Request::builder()
        .method(Method::GET)
        .uri(path_and_query(target.url()))
        .body(Empty::<Bytes>::new())
        .map_err(|error| {
            TransportFailure::invalid_input(format!("target path is not a request URI: {error}"))
        })?;
    send_request(
        stream,
        with_headers(request, request_headers(target, headers)?),
    )
    .await
}

async fn send_forward_proxy(
    stream: BoxIo,
    target: &PublicTarget,
    headers: HeaderMap,
    validated: SocketAddr,
    auth: Option<&HeaderValue>,
) -> Result<Response<Incoming>, TransportFailure> {
    let uri = format!(
        "http://{}{}",
        socket_authority(validated),
        path_and_query(target.url())
    );
    let request = Request::builder()
        .method(Method::GET)
        .uri(uri)
        .body(Empty::<Bytes>::new())
        .map_err(|error| {
            TransportFailure::invalid_input(format!("target path is not a request URI: {error}"))
        })?;
    let mut headers = request_headers(target, headers)?;
    if let Some(auth) = auth {
        headers.insert(PROXY_AUTHORIZATION, auth.clone());
    }
    send_request(stream, with_headers(request, headers)).await
}

fn with_headers(mut request: Request<Empty<Bytes>>, headers: HeaderMap) -> Request<Empty<Bytes>> {
    *request.headers_mut() = headers;
    request
}

async fn send_request(
    stream: BoxIo,
    request: Request<Empty<Bytes>>,
) -> Result<Response<Incoming>, TransportFailure> {
    let (mut sender, connection) = http1::handshake(TokioIo::new(stream))
        .await
        .map_err(|error| TransportFailure::transient(io::Error::other(error)))?;
    tokio::spawn(async move {
        if let Err(error) = connection.await {
            tracing::debug!(%error, "public HTTP connection ended");
        }
    });
    // The wait for response headers is deliberately unbounded here: it is response
    // latency rather than connection establishment, and the caller's request deadline is
    // the only budget that knows how long a page is worth waiting for.
    sender
        .send_request(request)
        .await
        .map_err(|error| TransportFailure::transient(io::Error::other(error)))
}

/// Tunnel HTTPS through an HTTP proxy, one validated address at a time.
///
/// The origin handshake happens inside the attempt, so a proxy that opens the tunnel and
/// then relays nothing yields to the next validated address instead of consuming the
/// caller's whole request.
async fn establish_tunnel(
    proxy: &Uri,
    target: &PublicTarget,
    tls: &Arc<ClientConfig>,
    addresses: &[SocketAddr],
    auth: Option<&HeaderValue>,
    attempt_timeout: Duration,
) -> Result<BoxIo, TransportFailure> {
    let server_name = tls_server_name(target)?;
    establish_in_turn(addresses, attempt_timeout, |address| {
        let tls = Arc::clone(tls);
        let server_name = server_name.clone();
        async move {
            let stream = connect_http_proxy(proxy, Arc::clone(&tls), attempt_timeout).await?;
            let stream = tunnel_http_proxy_once(stream, address, auth).await?;
            tls_connect(stream, server_name, tls, attempt_timeout).await
        }
    })
    .await
}

async fn tunnel_http_proxy_once(
    stream: BoxIo,
    address: SocketAddr,
    auth: Option<&HeaderValue>,
) -> Result<BoxIo, TransportFailure> {
    let authority = socket_authority(address);
    let request = Request::builder()
        .method(Method::CONNECT)
        .uri(authority.as_str())
        .header(HOST, authority.as_str())
        .body(Empty::<Bytes>::new())
        .map_err(|error| {
            TransportFailure::invalid_input(format!(
                "validated address is not an authority: {error}"
            ))
        })?;
    let mut request = request;
    if let Some(auth) = auth {
        request
            .headers_mut()
            .insert(PROXY_AUTHORIZATION, auth.clone());
    }
    let (mut sender, connection) = http1::handshake(TokioIo::new(stream))
        .await
        .map_err(|error| TransportFailure::transient(io::Error::other(error)))?;
    tokio::spawn(async move {
        if let Err(error) = connection.with_upgrades().await {
            tracing::debug!(%error, "public HTTP proxy tunnel ended");
        }
    });
    let response = sender
        .send_request(request)
        .await
        .map_err(|error| TransportFailure::transient(io::Error::other(error)))?;
    if response.status() != StatusCode::OK {
        return Err(connect_failure(response.status()));
    }
    let upgraded = hyper::upgrade::on(response)
        .await
        .map_err(|error| TransportFailure::transient(io::Error::other(error)))?;
    Ok(Box::new(TokioIo::new(upgraded)))
}

/// Classify a refused CONNECT.
///
/// Only three shapes are weather: the proxy asking for a later attempt (408, 429) and its
/// upstream failing (5xx other than 501). Everything else - a credential refusal, a proxy
/// that does not implement CONNECT, and a captive portal answering a redirect or a page
/// instead of `200` - is an answer, so an unlisted status reports permanent rather than
/// spending the caller's retry budget on it.
fn connect_failure(status: StatusCode) -> TransportFailure {
    let error = io::Error::other(format!(
        "proxy CONNECT to validated address failed with HTTP {status}"
    ));
    match status {
        StatusCode::REQUEST_TIMEOUT | StatusCode::TOO_MANY_REQUESTS => {
            TransportFailure::Transient(error)
        }
        StatusCode::NOT_IMPLEMENTED => TransportFailure::Permanent(error),
        // A proxy ACL is per destination, so another validated address for the same origin
        // may be permitted; do not cancel the remaining addresses on this one.
        StatusCode::FORBIDDEN | StatusCode::NOT_FOUND => {
            TransportFailure::PermanentForAddress(error)
        }
        status if status.is_server_error() => TransportFailure::Transient(error),
        _ => TransportFailure::Permanent(error),
    }
}

fn socket_authority(address: SocketAddr) -> String {
    address.to_string()
}

/// Reach the origin through a SOCKS4 proxy, one validated IPv4 address at a time.
async fn establish_socks4(
    proxy: &Uri,
    target: &PublicTarget,
    tls: &Arc<ClientConfig>,
    targets: &[SocketAddr],
    username: &str,
    attempt_timeout: Duration,
) -> Result<BoxIo, TransportFailure> {
    let ipv4 = targets
        .iter()
        .copied()
        .filter(SocketAddr::is_ipv4)
        .collect::<Vec<_>>();
    if ipv4.is_empty() {
        // The protocol cannot express this destination at all, which is a configuration
        // answer rather than an empty answer set.
        return Err(TransportFailure::Permanent(io::Error::new(
            io::ErrorKind::AddrNotAvailable,
            "SOCKS4 requires a validated IPv4 target",
        )));
    }
    establish_in_turn(&ipv4, attempt_timeout, |address| {
        let tls = Arc::clone(tls);
        async move {
            let stream = connect_socks4_once(proxy, address, username, attempt_timeout).await?;
            secure_origin_if_needed(stream, target, tls, attempt_timeout).await
        }
    })
    .await
}

async fn connect_socks4_once(
    proxy: &Uri,
    target: SocketAddr,
    username: &str,
    attempt_timeout: Duration,
) -> Result<BoxIo, TransportFailure> {
    let IpAddr::V4(ip) = target.ip() else {
        return Err(TransportFailure::Permanent(io::Error::new(
            io::ErrorKind::AddrNotAvailable,
            "SOCKS4 requires a validated IPv4 target",
        )));
    };
    let mut stream = connect_proxy_tcp(proxy, 1080, attempt_timeout).await?;
    let mut request = Vec::with_capacity(9 + username.len());
    request.extend_from_slice(&[4, 1]);
    request.extend_from_slice(&target.port().to_be_bytes());
    request.extend_from_slice(&ip.octets());
    request.extend_from_slice(username.as_bytes());
    request.push(0);
    stream
        .write_all(&request)
        .await
        .map_err(TransportFailure::transient)?;
    let mut response = [0_u8; 8];
    stream
        .read_exact(&mut response)
        .await
        .map_err(TransportFailure::transient)?;
    if response[1] == 0x5a {
        return Ok(stream);
    }
    Err(socks4_reply_failure(response[1]))
}

/// Classify a SOCKS4 reply code.
///
/// `0x5b` is the protocol's generic "request failed", which covers an unreachable
/// destination, so it stays retryable. `0x5c` and `0x5d` are identd refusals that no
/// retry resolves, and a code RFC 1928's predecessor never defined comes from a
/// non-conformant proxy, so it reports permanent rather than looping on backoff.
fn socks4_reply_failure(reply: u8) -> TransportFailure {
    let error = io::Error::other(format!(
        "SOCKS4 proxy rejected validated address with status 0x{reply:02x}"
    ));
    match reply {
        0x5b => TransportFailure::Transient(error),
        _ => TransportFailure::Permanent(error),
    }
}

/// Reach the origin through a SOCKS5 proxy, one validated address at a time.
async fn establish_socks5(
    proxy: &Uri,
    target: &PublicTarget,
    tls: &Arc<ClientConfig>,
    targets: &[SocketAddr],
    credentials: Option<&(String, String)>,
    attempt_timeout: Duration,
) -> Result<BoxIo, TransportFailure> {
    establish_in_turn(targets, attempt_timeout, |address| {
        let tls = Arc::clone(tls);
        async move {
            let stream = connect_socks5_once(proxy, address, credentials, attempt_timeout).await?;
            secure_origin_if_needed(stream, target, tls, attempt_timeout).await
        }
    })
    .await
}

async fn connect_socks5_once(
    proxy: &Uri,
    target: SocketAddr,
    credentials: Option<&(String, String)>,
    attempt_timeout: Duration,
) -> Result<BoxIo, TransportFailure> {
    let mut stream = connect_proxy_tcp(proxy, 1080, attempt_timeout).await?;
    let greeting: &[u8] = if credentials.is_some() {
        &[5, 2, 0, 2]
    } else {
        &[5, 1, 0]
    };
    stream
        .write_all(greeting)
        .await
        .map_err(TransportFailure::transient)?;
    let mut selected = [0_u8; 2];
    stream
        .read_exact(&mut selected)
        .await
        .map_err(TransportFailure::transient)?;
    if selected[0] != 5 || selected[1] == 0xff {
        return Err(TransportFailure::Permanent(io::Error::other(
            "SOCKS5 proxy rejected authentication methods",
        )));
    }
    match selected[1] {
        0 => {}
        2 => {
            let (username, password) = credentials.ok_or_else(|| {
                TransportFailure::Permanent(io::Error::other("SOCKS5 proxy requires credentials"))
            })?;
            if username.len() > u8::MAX as usize || password.len() > u8::MAX as usize {
                return Err(TransportFailure::invalid_input(
                    "SOCKS5 credentials exceed protocol limits",
                ));
            }
            let mut auth = Vec::with_capacity(3 + username.len() + password.len());
            auth.extend_from_slice(&[1, username.len() as u8]);
            auth.extend_from_slice(username.as_bytes());
            auth.push(password.len() as u8);
            auth.extend_from_slice(password.as_bytes());
            stream
                .write_all(&auth)
                .await
                .map_err(TransportFailure::transient)?;
            let mut result = [0_u8; 2];
            stream
                .read_exact(&mut result)
                .await
                .map_err(TransportFailure::transient)?;
            if result != [1, 0] {
                return Err(TransportFailure::Permanent(io::Error::other(
                    "SOCKS5 proxy authentication failed",
                )));
            }
        }
        method => {
            return Err(TransportFailure::Permanent(io::Error::other(format!(
                "SOCKS5 proxy selected unsupported authentication method {method}"
            ))));
        }
    }

    let mut request = Vec::with_capacity(22);
    request.extend_from_slice(&[5, 1, 0]);
    match target.ip() {
        IpAddr::V4(address) => {
            request.push(1);
            request.extend_from_slice(&address.octets());
        }
        IpAddr::V6(address) => {
            request.push(4);
            request.extend_from_slice(&address.octets());
        }
    }
    request.extend_from_slice(&target.port().to_be_bytes());
    stream
        .write_all(&request)
        .await
        .map_err(TransportFailure::transient)?;

    let mut head = [0_u8; 4];
    stream
        .read_exact(&mut head)
        .await
        .map_err(TransportFailure::transient)?;
    if head[0] != 5 || head[1] != 0 {
        return Err(socks5_reply_failure(head[1]));
    }
    let address_len = match head[3] {
        1 => 4,
        4 => 16,
        3 => {
            let mut length = [0_u8; 1];
            stream
                .read_exact(&mut length)
                .await
                .map_err(TransportFailure::transient)?;
            usize::from(length[0])
        }
        _ => {
            return Err(TransportFailure::Permanent(io::Error::other(
                "SOCKS5 proxy returned invalid address type",
            )));
        }
    };
    let mut ignored = vec![0_u8; address_len + 2];
    stream
        .read_exact(&mut ignored)
        .await
        .map_err(TransportFailure::transient)?;
    Ok(stream)
}

/// Classify a SOCKS5 reply code (RFC 1928 section 6).
///
/// `0x02` is the proxy's ruleset and `0x08` is an address family it will not carry: both
/// are decided per destination address, so the remaining validated addresses still get an
/// attempt - a SOCKS5 proxy without IPv6 egress must not fail an origin that also has an
/// A record. `0x07` is the proxy's own capability. A code the RFC does not define comes
/// from a non-conformant proxy and reports permanent rather than looping on backoff.
fn socks5_reply_failure(reply: u8) -> TransportFailure {
    let error = io::Error::other(format!(
        "SOCKS5 proxy rejected validated address with status 0x{reply:02x}"
    ));
    match reply {
        0x01 | 0x03 | 0x04 | 0x05 | 0x06 => TransportFailure::Transient(error),
        0x02 | 0x08 => TransportFailure::PermanentForAddress(error),
        _ => TransportFailure::Permanent(error),
    }
}

async fn connect_proxy_tcp(
    proxy: &Uri,
    default_port: u16,
    attempt_timeout: Duration,
) -> Result<BoxIo, TransportFailure> {
    let host = proxy
        .host()
        .ok_or_else(|| TransportFailure::invalid_input("proxy has no host"))?;
    connect_host(
        host,
        proxy.port_u16().unwrap_or(default_port),
        attempt_timeout,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::CertificateError;

    /// `BoxIo` is not `Debug`, so `expect_err` cannot report the unexpected success.
    fn expect_failure(result: Result<BoxIo, TransportFailure>, why: &str) -> TransportFailure {
        match result {
            Ok(_) => panic!("{why}"),
            Err(failure) => failure,
        }
    }

    /// One parse of the platform trust store, and therefore one client session store.
    ///
    /// The pointer equality is the whole mechanism: it is what makes the trust parse
    /// happen once, and it is also what makes TLS resumption state process-wide. A change
    /// that gives each transport its own `ClientConfig` to isolate resumption would fail
    /// here, which is the intended signal - see `shared_tls_config`.
    #[test]
    fn platform_trust_material_is_parsed_once_per_process() {
        let first = SessionTransport::from_process();
        let second = SessionTransport::from_process();
        assert!(
            Arc::ptr_eq(&first.tls, &second.tls),
            "every client must share one parse of the platform trust store"
        );
    }

    #[test]
    fn ipv6_literal_targets_produce_an_ip_server_name() {
        let target = PublicTarget::parse("https://[2606:4700:4700::1111]/path")
            .expect("IPv6 literal target");
        let name = tls_server_name(&target).expect("SNI name for an IPv6 literal");
        assert!(matches!(name, ServerName::IpAddress(_)), "{name:?}");
        assert_eq!(
            origin_authority(&target).expect("origin authority"),
            "[2606:4700:4700::1111]"
        );
    }

    #[test]
    fn hostname_targets_keep_their_dns_server_name_and_authority() {
        let target = PublicTarget::parse("https://origin.example:8443/path").expect("DNS target");
        let name = tls_server_name(&target).expect("SNI name for a hostname");
        assert!(matches!(name, ServerName::DnsName(_)), "{name:?}");
        assert_eq!(
            origin_authority(&target).expect("origin authority"),
            "origin.example:8443"
        );
        assert_eq!(unbracketed("[::1]"), "::1");
        assert_eq!(unbracketed("proxy.example"), "proxy.example");
    }

    #[test]
    fn address_families_alternate_from_the_resolver_preference() {
        let first_v6: SocketAddr = "[2606:4700:4700::1111]:443".parse().expect("IPv6 address");
        let second_v6: SocketAddr = "[2606:4700:4700::1112]:443".parse().expect("IPv6 address");
        let first_v4: SocketAddr = "93.184.216.34:443".parse().expect("IPv4 address");
        let second_v4: SocketAddr = "93.184.216.35:443".parse().expect("IPv4 address");
        assert_eq!(
            interleave_families(&[first_v6, second_v6, first_v4, second_v4]),
            vec![first_v6, first_v4, second_v6, second_v4]
        );
        assert_eq!(
            interleave_families(&[first_v4, first_v6]),
            vec![first_v4, first_v6]
        );
        assert!(interleave_families(&[]).is_empty());
    }

    #[tokio::test]
    async fn a_blackholed_address_yields_to_the_next_validated_address() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test listener");
        let reachable = listener.local_addr().expect("listener address");
        let blackholed: SocketAddr = "93.184.216.34:443".parse().expect("IPv4 address");
        let attempted = std::sync::Mutex::new(Vec::new());
        let stream = establish_in_turn(
            &[blackholed, reachable],
            Duration::from_millis(50),
            |address| {
                attempted.lock().expect("attempt log").push(address);
                async move {
                    if address == blackholed {
                        // A dropped SYN, a TLS handshake that stalls after the SYN, and a
                        // proxy that never answers CONNECT all look exactly like this.
                        std::future::pending::<Result<BoxIo, TransportFailure>>().await
                    } else {
                        Ok(Box::new(
                            TcpStream::connect(address)
                                .await
                                .map_err(TransportFailure::transient)?,
                        ) as BoxIo)
                    }
                }
            },
        )
        .await
        .expect("the reachable address must still be attempted");
        drop(stream);
        assert_eq!(
            *attempted.lock().expect("attempt log"),
            vec![blackholed, reachable]
        );
    }

    /// The production wrapper must carry the address loop, not just the injected helper.
    ///
    /// The advancement this pins is refusal-driven: a refused address fails instantly, so
    /// this test says nothing about the establishment deadlines. Those are covered by
    /// `a_blackholed_address_yields_to_the_next_validated_address`,
    /// `a_large_dns_answer_cannot_multiply_the_establishment_budget`, and
    /// `public_http::tests::a_stalled_handshake_yields_to_the_next_validated_address`.
    #[tokio::test]
    async fn a_refused_address_does_not_end_the_shipped_address_walk() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test listener");
        let reachable = listener.local_addr().expect("listener address");
        let refusing = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a port to learn a free one");
        let refused = refusing.local_addr().expect("listener address");
        drop(refusing);
        let stream = connect_any(&[refused, reachable], ESTABLISH_ATTEMPT_TIMEOUT)
            .await
            .expect("a refused address must not end the attempt sequence");
        // The reachable listener is the only address that can answer, so observing its
        // accepted connection is what proves the walk advanced past the refused address.
        let (_accepted, peer) = listener
            .accept()
            .await
            .expect("the walk must reach the reachable address");
        assert_eq!(peer.ip(), reachable.ip());
        drop(stream);
    }

    #[tokio::test]
    async fn an_empty_address_set_stays_retryable() {
        let failure = expect_failure(
            establish_in_turn(&[], Duration::from_millis(50), |_| async {
                unreachable!("there is no address to attempt")
            })
            .await,
            "no address cannot produce a connection",
        );
        assert!(
            !failure.is_permanent(),
            "an empty answer set is not a peer decision: {:?}",
            failure.into_io()
        );
    }

    #[tokio::test]
    async fn a_per_destination_refusal_leaves_the_other_addresses_alone() {
        let first: SocketAddr = "93.184.216.34:443".parse().expect("IPv4 address");
        let second: SocketAddr = "93.184.216.35:443".parse().expect("IPv4 address");
        let attempted = std::sync::Mutex::new(Vec::new());
        let failure = expect_failure(
            establish_in_turn(&[first, second], Duration::from_secs(5), |address| {
                attempted.lock().expect("attempt log").push(address);
                async move { Err(socks5_reply_failure(0x02)) }
            })
            .await,
            "both addresses were refused",
        );
        assert_eq!(
            *attempted.lock().expect("attempt log"),
            vec![first, second],
            "a per-destination refusal must not cancel the remaining validated addresses"
        );
        assert!(
            failure.is_permanent(),
            "every address answered with a refusal: {:?}",
            failure.into_io()
        );
    }

    #[tokio::test]
    async fn a_request_level_refusal_cancels_the_remaining_addresses() {
        let first: SocketAddr = "93.184.216.34:443".parse().expect("IPv4 address");
        let second: SocketAddr = "93.184.216.35:443".parse().expect("IPv4 address");
        let attempted = std::sync::Mutex::new(Vec::new());
        let failure = expect_failure(
            establish_in_turn(&[first, second], Duration::from_secs(5), |address| {
                attempted.lock().expect("attempt log").push(address);
                async move { Err(connect_failure(StatusCode::PROXY_AUTHENTICATION_REQUIRED)) }
            })
            .await,
            "the proxy refused the credentials",
        );
        assert_eq!(
            *attempted.lock().expect("attempt log"),
            vec![first],
            "a refused credential is decided before the destination address matters"
        );
        assert!(failure.is_permanent());
    }

    /// No input can raise the sequence ceiling, and no value can overflow it.
    ///
    /// The multiplication is the direction that matters: the total is derived from the
    /// per-address budget, which is itself clamped, so tightening one tightens both. The
    /// `Duration::MAX` case is the arithmetic guard - a saturating multiply keeps a
    /// pathological value from wrapping or panicking into a larger budget.
    #[test]
    fn no_per_address_budget_can_raise_the_sequence_ceiling() {
        assert_eq!(
            total_establish_budget(Duration::from_secs(1)),
            Duration::from_secs(3),
            "a caller that tightens the per-address budget tightens the sequence with it"
        );
        assert_eq!(
            total_establish_budget(Duration::from_millis(100)),
            Duration::from_millis(300)
        );
        assert_eq!(
            total_establish_budget(Duration::from_secs(600)),
            Duration::from_secs(30),
            "the ceiling holds however large the per-address budget is"
        );
        assert_eq!(
            total_establish_budget(Duration::MAX),
            Duration::from_secs(30),
            "the multiply must saturate rather than wrap into a larger budget"
        );
        assert!(
            total_establish_budget(ESTABLISH_ATTEMPT_TIMEOUT) >= ESTABLISH_ATTEMPT_TIMEOUT,
            "a single address must always be able to spend its whole per-address budget"
        );
    }

    /// The reviewer's input: one proxy-denied address plus one address that only stalls.
    ///
    /// A `PermanentForAddress` answer is the peer's decision about that destination. The
    /// DNS answer that supplies the other addresses is chosen by the target's own zone, so
    /// letting an unrelated address's stall rewrite the denial into weather would hand the
    /// retry decision to whoever controls that answer. Both orders are asserted because the
    /// verdict must be a property of the set, not of which address happened to answer last.
    #[tokio::test]
    async fn a_denied_address_is_not_laundered_by_an_unrelated_stalled_address() {
        let denied: SocketAddr = "93.184.216.34:443".parse().expect("IPv4 address");
        let stalled: SocketAddr = "93.184.216.35:443".parse().expect("IPv4 address");
        let alone = expect_failure(
            establish_in_turn(&[denied], Duration::from_millis(200), |_| async {
                Err(connect_failure(StatusCode::FORBIDDEN))
            })
            .await,
            "a denied destination cannot connect",
        );
        assert!(
            alone.is_permanent(),
            "a CONNECT 403 alone is a decision: {:?}",
            alone.into_io()
        );
        for order in [[denied, stalled], [stalled, denied]] {
            let failure = expect_failure(
                establish_in_turn(&order, Duration::from_millis(200), |address| async move {
                    if address == denied {
                        Err(connect_failure(StatusCode::FORBIDDEN))
                    } else {
                        std::future::pending::<Result<BoxIo, TransportFailure>>().await
                    }
                })
                .await,
                "no address produced a connection",
            );
            let permanent = failure.is_permanent();
            let rendered = failure.into_io().to_string();
            assert!(
                permanent,
                "a stalled sibling must not make a denied destination retryable: {rendered} \
                 (order {order:?})"
            );
            assert!(
                rendered.contains("403"),
                "the surfaced failure must be the denial itself, not a rewritten class: \
                 {rendered}"
            );
        }
    }

    /// The same laundering through SOCKS5 reply `0x02`, and through a sibling that answers.
    ///
    /// `ConnectionRefused` is a real answer rather than a stall, so this is the harder
    /// direction: even a genuine transient answer from another address must not speak for
    /// the address the proxy's ruleset denied.
    #[tokio::test]
    async fn a_socks5_ruleset_denial_is_not_laundered_by_another_address() {
        let refused: SocketAddr = "93.184.216.34:443".parse().expect("IPv4 address");
        let denied: SocketAddr = "93.184.216.35:443".parse().expect("IPv4 address");
        let failure = expect_failure(
            establish_in_turn(
                &[refused, denied],
                Duration::from_millis(200),
                |address| async move {
                    if address == refused {
                        Err(TransportFailure::transient(io::Error::from(
                            io::ErrorKind::ConnectionRefused,
                        )))
                    } else {
                        Err(socks5_reply_failure(0x02))
                    }
                },
            )
            .await,
            "no address produced a connection",
        );
        let permanent = failure.is_permanent();
        let rendered = failure.into_io().to_string();
        assert!(
            permanent,
            "a ruleset denial is the peer's answer for that destination: {rendered}"
        );
        assert!(
            rendered.contains("0x02"),
            "the surfaced failure must name the denial: {rendered}"
        );
    }

    /// A peer-chosen address count must not multiply the establishment budget.
    ///
    /// 30 addresses that each stall is the shape a hostile or compromised authoritative
    /// server produces; with a per-address budget alone the sequence costs 30 x that budget.
    #[tokio::test]
    async fn a_large_dns_answer_cannot_multiply_the_establishment_budget() {
        let addresses = (1_u8..=30)
            .map(|host| {
                format!("93.184.216.{host}:443")
                    .parse::<SocketAddr>()
                    .expect("IPv4 address")
            })
            .collect::<Vec<_>>();
        let attempted = std::sync::Mutex::new(0_usize);
        let started = Instant::now();
        let failure = expect_failure(
            establish_in_turn(&addresses, Duration::from_millis(200), |_| {
                *attempted.lock().expect("attempt count") += 1;
                async { std::future::pending::<Result<BoxIo, TransportFailure>>().await }
            })
            .await,
            "every address stalled",
        );
        let elapsed = started.elapsed();
        let attempts = *attempted.lock().expect("attempt count");
        assert_eq!(
            attempts, 3,
            "the total establishment budget is 3 stalled addresses' worth, not 30"
        );
        assert!(
            elapsed < Duration::from_millis(1_500),
            "30 stalled addresses must not hold the caller for 30 attempts: {elapsed:?}"
        );
        let permanent = failure.is_permanent();
        let rendered = failure.into_io().to_string();
        assert!(
            !permanent,
            "our own budget expiring is not a peer decision: {rendered}"
        );
        assert!(
            rendered.contains("gave up"),
            "an abandoned walk must say so rather than looking like a single timeout: \
             {rendered}"
        );
    }

    /// 64 instantly denied addresses: the walk is capped, and the denial still wins.
    #[tokio::test]
    async fn a_large_dns_answer_cannot_multiply_proxy_connections() {
        let addresses = (1_u8..=64)
            .map(|host| {
                format!("93.184.216.{host}:443")
                    .parse::<SocketAddr>()
                    .expect("IPv4 address")
            })
            .collect::<Vec<_>>();
        let attempted = std::sync::Mutex::new(0_usize);
        let failure = expect_failure(
            establish_in_turn(&addresses, Duration::from_millis(200), |_| {
                *attempted.lock().expect("attempt count") += 1;
                async { Err(connect_failure(StatusCode::FORBIDDEN)) }
            })
            .await,
            "every address was denied",
        );
        assert_eq!(
            *attempted.lock().expect("attempt count"),
            8,
            "one peer-supplied answer must not turn into 64 proxy round trips"
        );
        assert!(
            failure.is_permanent(),
            "the addresses that answered were all denied: {:?}",
            failure.into_io()
        );
    }

    /// Weather at every address, and no peer decision anywhere: still retryable.
    ///
    /// This is the direction the denial rule must not swallow. Nothing in this sequence is
    /// an answer about a destination, so the request keeps its retry.
    #[tokio::test]
    async fn a_sequence_without_a_peer_decision_stays_retryable() {
        let refused: SocketAddr = "93.184.216.34:443".parse().expect("IPv4 address");
        let unreachable: SocketAddr = "93.184.216.35:443".parse().expect("IPv4 address");
        let failure = expect_failure(
            establish_in_turn(
                &[refused, unreachable],
                Duration::from_secs(5),
                |address| async move {
                    if address == refused {
                        Err(TransportFailure::transient(io::Error::from(
                            io::ErrorKind::ConnectionRefused,
                        )))
                    } else {
                        Err(TransportFailure::transient(io::Error::from(
                            io::ErrorKind::HostUnreachable,
                        )))
                    }
                },
            )
            .await,
            "no address produced a connection",
        );
        assert!(
            !failure.is_permanent(),
            "a socket failure is not a decision about the destination: {:?}",
            failure.into_io()
        );
    }

    /// A real rustls handshake against a peer that answers a ClientHello with a page.
    ///
    /// This is the shape a captive portal or an interception box produces on 443, and the
    /// classification has to come out of the handshake rather than out of a hand-built
    /// error value, because a hand-built value can only ever restate the predicate under
    /// test.
    #[tokio::test]
    async fn an_intercepted_tls_port_is_a_permanent_failure() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test listener");
        let address = listener.local_addr().expect("listener address");
        let portal = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept the ClientHello");
            let mut discard = [0_u8; 4096];
            let _ = stream.read(&mut discard).await;
            let _ = stream
                .write_all(b"HTTP/1.1 302 Found\r\nLocation: http://portal.example/\r\n\r\n")
                .await;
            let _ = stream.flush().await;
            tokio::time::sleep(Duration::from_secs(5)).await;
        });
        let stream: BoxIo = Box::new(
            TcpStream::connect(address)
                .await
                .expect("connect to the test listener"),
        );
        let failure = expect_failure(
            tls_connect(
                stream,
                server_name("origin.example").expect("SNI name"),
                shared_tls_config(),
                ESTABLISH_ATTEMPT_TIMEOUT,
            )
            .await,
            "a plaintext page cannot complete a TLS handshake",
        );
        let permanent = failure.is_permanent();
        assert!(
            permanent,
            "an intercepted TLS port must not be retried on backoff: {:?}",
            failure.into_io()
        );
        portal.abort();
    }

    #[test]
    fn handshake_classification_covers_more_than_the_certificate_class() {
        let permanent = [
            rustls::Error::InvalidCertificate(CertificateError::UnknownIssuer),
            rustls::Error::InvalidCertificate(CertificateError::NotValidForName),
            rustls::Error::InvalidMessage(rustls::InvalidMessage::InvalidContentType),
            rustls::Error::PeerMisbehaved(rustls::PeerMisbehaved::TooMuchEarlyDataReceived),
            rustls::Error::DecryptError,
            rustls::Error::EncryptError,
            rustls::Error::PeerSentOversizedRecord,
            rustls::Error::HandshakeNotComplete,
            rustls::Error::BadMaxFragmentSize,
            rustls::Error::NoApplicationProtocol,
            rustls::Error::AlertReceived(rustls::AlertDescription::HandshakeFailure),
            rustls::Error::AlertReceived(rustls::AlertDescription::AccessDenied),
            rustls::Error::General("interception".to_owned()),
        ];
        for error in permanent {
            assert!(
                is_permanent_tls_error(&error),
                "{error:?} is decided by the wire or by this configuration, not by weather"
            );
        }
        let transient = [
            // The peer says the fault is its own or that it is going away.
            rustls::Error::AlertReceived(rustls::AlertDescription::InternalError),
            rustls::Error::AlertReceived(rustls::AlertDescription::UserCanceled),
            rustls::Error::AlertReceived(rustls::AlertDescription::CloseNotify),
            // Revocation state could not be reached, which is not a bad certificate. The
            // platform verifier funnels an unreachable macOS or Windows OCSP responder
            // into `Other`, so that bucket keeps the retry it has today.
            rustls::Error::InvalidCertificate(CertificateError::UnknownRevocationStatus),
            rustls::Error::InvalidCertificate(CertificateError::ExpiredRevocationList),
            rustls::Error::InvalidCertificate(CertificateError::Other(rustls::OtherError(
                Arc::new(io::Error::from(io::ErrorKind::TimedOut)),
            ))),
            rustls::Error::FailedToGetCurrentTime,
            rustls::Error::FailedToGetRandomBytes,
        ];
        for error in transient {
            assert!(
                !is_permanent_tls_error(&error),
                "{error:?} can answer differently on the next attempt"
            );
        }
    }

    #[test]
    fn a_socket_failure_during_a_handshake_stays_retryable() {
        let reset = classify_tls_error(io::Error::from(io::ErrorKind::ConnectionReset));
        assert!(
            !reset.is_permanent(),
            "a socket failure during the handshake is not a rustls decision"
        );
        let rejected = classify_tls_error(io::Error::new(
            io::ErrorKind::InvalidData,
            rustls::Error::InvalidCertificate(CertificateError::UnknownIssuer),
        ));
        assert!(rejected.is_permanent());
    }

    #[test]
    fn a_refused_connect_separates_the_request_from_the_destination() {
        for status in [
            StatusCode::PROXY_AUTHENTICATION_REQUIRED,
            StatusCode::UNAUTHORIZED,
            StatusCode::NOT_IMPLEMENTED,
            StatusCode::FOUND,
            StatusCode::NO_CONTENT,
        ] {
            let failure = connect_failure(status);
            assert!(failure.is_permanent(), "{status} is a refusal");
            assert!(
                failure.short_circuits(),
                "{status} is decided before the destination address matters"
            );
        }
        for status in [StatusCode::FORBIDDEN, StatusCode::NOT_FOUND] {
            let failure = connect_failure(status);
            assert!(failure.is_permanent(), "{status} is a refusal");
            assert!(
                !failure.short_circuits(),
                "a proxy ACL is per destination, so the other addresses still get a turn"
            );
        }
        for status in [
            StatusCode::BAD_GATEWAY,
            StatusCode::GATEWAY_TIMEOUT,
            StatusCode::INTERNAL_SERVER_ERROR,
            StatusCode::REQUEST_TIMEOUT,
            StatusCode::TOO_MANY_REQUESTS,
        ] {
            assert!(
                !connect_failure(status).is_permanent(),
                "{status} reports the proxy's own state"
            );
        }
    }

    #[test]
    fn socks_replies_separate_the_ruleset_from_an_unreachable_destination() {
        for reply in [0x01, 0x03, 0x04, 0x05, 0x06] {
            assert!(
                !socks5_reply_failure(reply).is_permanent(),
                "SOCKS5 0x{reply:02x} reports the network or the proxy's own state"
            );
        }
        for reply in [0x02, 0x08] {
            let failure = socks5_reply_failure(reply);
            assert!(failure.is_permanent(), "SOCKS5 0x{reply:02x} is a refusal");
            assert!(
                !failure.short_circuits(),
                "SOCKS5 0x{reply:02x} names one destination, not the request"
            );
        }
        for reply in [0x07, 0x09, 0xff] {
            let failure = socks5_reply_failure(reply);
            assert!(failure.is_permanent());
            assert!(
                failure.short_circuits(),
                "a reply RFC 1928 does not define must not loop on backoff"
            );
        }
        assert!(!socks4_reply_failure(0x5b).is_permanent());
        for reply in [0x5c, 0x5d, 0x00] {
            assert!(socks4_reply_failure(reply).is_permanent());
        }
    }
}
