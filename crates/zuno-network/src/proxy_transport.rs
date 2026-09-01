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
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
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

#[derive(Clone)]
pub(crate) struct SessionTransport {
    proxies: Arc<ProxyEnvironment>,
    tls: Arc<ClientConfig>,
}

impl fmt::Debug for SessionTransport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionTransport")
            .field("proxies", &self.proxies)
            .finish_non_exhaustive()
    }
}

impl SessionTransport {
    pub(crate) fn from_process() -> Self {
        let mut tls = ClientConfig::with_platform_verifier()
            .expect("platform TLS verifier must initialize for public HTTP");
        tls.alpn_protocols = vec![b"http/1.1".to_vec()];
        Self {
            proxies: Arc::new(ProxyEnvironment::from_process()),
            tls: Arc::new(tls),
        }
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
    ) -> Result<(Response<Incoming>, RouteKind), io::Error> {
        let route = self.proxies.route(target)?;
        let kind = route.kind();
        let response = match route {
            Route::Direct { .. } => {
                let stream = connect_any(direct_addresses).await?;
                let stream = secure_origin_if_needed(stream, target, Arc::clone(&self.tls)).await?;
                send_origin(stream, target, headers).await?
            }
            Route::HttpProxy { proxy, auth } => {
                if target.url().scheme() == "https" {
                    let stream = tunnel_http_proxy(
                        &proxy,
                        Arc::clone(&self.tls),
                        validated_addresses,
                        auth.as_ref(),
                    )
                    .await?;
                    let stream =
                        tls_connect(stream, target_host(target)?, Arc::clone(&self.tls)).await?;
                    send_origin(stream, target, headers).await?
                } else {
                    let stream = connect_http_proxy(&proxy, Arc::clone(&self.tls)).await?;
                    send_forward_proxy(
                        stream,
                        target,
                        headers,
                        validated_addresses
                            .first()
                            .copied()
                            .ok_or_else(|| io::Error::other("no validated target address"))?,
                        auth.as_ref(),
                    )
                    .await?
                }
            }
            Route::Socks4Proxy { proxy, username } => {
                let stream = connect_socks4(&proxy, validated_addresses, &username).await?;
                let stream = secure_origin_if_needed(stream, target, Arc::clone(&self.tls)).await?;
                send_origin(stream, target, headers).await?
            }
            Route::Socks5Proxy { proxy, credentials } => {
                let stream =
                    connect_socks5(&proxy, validated_addresses, credentials.as_ref()).await?;
                let stream = secure_origin_if_needed(stream, target, Arc::clone(&self.tls)).await?;
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
    let host = target_host(target)?;
    let rendered_host = if host
        .parse::<IpAddr>()
        .is_ok_and(|address| address.is_ipv6())
    {
        format!("[{host}]")
    } else {
        host.to_owned()
    };
    Ok(match target.url().port() {
        Some(port) => format!("{rendered_host}:{port}"),
        None => rendered_host,
    })
}

fn path_and_query(url: &Url) -> &str {
    let path = &url[url::Position::BeforePath..url::Position::AfterQuery];
    if path.is_empty() { "/" } else { path }
}

fn request_headers(target: &PublicTarget, mut headers: HeaderMap) -> Result<HeaderMap, io::Error> {
    headers.remove(PROXY_AUTHORIZATION);
    headers.insert(
        HOST,
        HeaderValue::from_str(&origin_authority(target)?)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?,
    );
    Ok(headers)
}

async fn connect_any(addresses: &[SocketAddr]) -> Result<BoxIo, io::Error> {
    let mut last = None;
    for address in addresses {
        match TcpStream::connect(address).await {
            Ok(stream) => return Ok(Box::new(stream)),
            Err(error) => last = Some(error),
        }
    }
    Err(last.unwrap_or_else(|| io::Error::other("no connector addresses")))
}

async fn connect_host(host: &str, port: u16) -> Result<BoxIo, io::Error> {
    let addresses = tokio::net::lookup_host((host, port)).await?;
    let addresses = addresses.collect::<Vec<_>>();
    connect_any(&addresses).await
}

async fn tls_connect(
    stream: BoxIo,
    host: &str,
    config: Arc<ClientConfig>,
) -> Result<BoxIo, io::Error> {
    let server_name = ServerName::try_from(host.to_owned()).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid TLS server name: {error}"),
        )
    })?;
    let stream = TlsConnector::from(config)
        .connect(server_name, stream)
        .await
        .map_err(io::Error::other)?;
    Ok(Box::new(stream))
}

async fn secure_origin_if_needed(
    stream: BoxIo,
    target: &PublicTarget,
    config: Arc<ClientConfig>,
) -> Result<BoxIo, io::Error> {
    if target.url().scheme() == "https" {
        tls_connect(stream, target_host(target)?, config).await
    } else {
        Ok(stream)
    }
}

async fn connect_http_proxy(proxy: &Uri, tls: Arc<ClientConfig>) -> Result<BoxIo, io::Error> {
    let host = proxy
        .host()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "proxy has no host"))?;
    let port = proxy.port_u16().unwrap_or_else(|| {
        if proxy.scheme() == Some(&http::uri::Scheme::HTTPS) {
            443
        } else {
            80
        }
    });
    let stream = connect_host(host, port).await?;
    if proxy.scheme() == Some(&http::uri::Scheme::HTTPS) {
        tls_connect(stream, host, tls).await
    } else {
        Ok(stream)
    }
}

async fn send_origin(
    stream: BoxIo,
    target: &PublicTarget,
    headers: HeaderMap,
) -> Result<Response<Incoming>, io::Error> {
    let request = Request::builder()
        .method(Method::GET)
        .uri(path_and_query(target.url()))
        .body(Empty::<Bytes>::new())
        .map_err(io::Error::other)?;
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
) -> Result<Response<Incoming>, io::Error> {
    let uri = format!(
        "http://{}{}",
        socket_authority(validated),
        path_and_query(target.url())
    );
    let request = Request::builder()
        .method(Method::GET)
        .uri(uri)
        .body(Empty::<Bytes>::new())
        .map_err(io::Error::other)?;
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
) -> Result<Response<Incoming>, io::Error> {
    let (mut sender, connection) = http1::handshake(TokioIo::new(stream))
        .await
        .map_err(io::Error::other)?;
    tokio::spawn(async move {
        if let Err(error) = connection.await {
            tracing::debug!(%error, "public HTTP connection ended");
        }
    });
    sender.send_request(request).await.map_err(io::Error::other)
}

async fn tunnel_http_proxy(
    proxy: &Uri,
    tls: Arc<ClientConfig>,
    addresses: &[SocketAddr],
    auth: Option<&HeaderValue>,
) -> Result<BoxIo, io::Error> {
    let mut last = None;
    for address in addresses {
        let stream = connect_http_proxy(proxy, Arc::clone(&tls)).await?;
        match tunnel_http_proxy_once(stream, *address, auth).await {
            Ok(stream) => return Ok(stream),
            Err(error) => last = Some(error),
        }
    }
    Err(last.unwrap_or_else(|| io::Error::other("no validated target address")))
}

async fn tunnel_http_proxy_once(
    stream: BoxIo,
    address: SocketAddr,
    auth: Option<&HeaderValue>,
) -> Result<BoxIo, io::Error> {
    let authority = socket_authority(address);
    let request = Request::builder()
        .method(Method::CONNECT)
        .uri(authority.as_str())
        .header(HOST, authority.as_str())
        .body(Empty::<Bytes>::new())
        .map_err(io::Error::other)?;
    let mut request = request;
    if let Some(auth) = auth {
        request
            .headers_mut()
            .insert(PROXY_AUTHORIZATION, auth.clone());
    }
    let (mut sender, connection) = http1::handshake(TokioIo::new(stream))
        .await
        .map_err(io::Error::other)?;
    tokio::spawn(async move {
        if let Err(error) = connection.with_upgrades().await {
            tracing::debug!(%error, "public HTTP proxy tunnel ended");
        }
    });
    let response = sender
        .send_request(request)
        .await
        .map_err(io::Error::other)?;
    if response.status() != StatusCode::OK {
        return Err(io::Error::other(format!(
            "proxy CONNECT to validated address failed with HTTP {}",
            response.status()
        )));
    }
    let upgraded = hyper::upgrade::on(response)
        .await
        .map_err(io::Error::other)?;
    Ok(Box::new(TokioIo::new(upgraded)))
}

fn socket_authority(address: SocketAddr) -> String {
    address.to_string()
}

async fn connect_socks4(
    proxy: &Uri,
    targets: &[SocketAddr],
    username: &str,
) -> Result<BoxIo, io::Error> {
    let mut last = None;
    for target in targets {
        let IpAddr::V4(ip) = target.ip() else {
            continue;
        };
        let mut stream = connect_proxy_tcp(proxy, 1080).await?;
        let mut request = Vec::with_capacity(9 + username.len());
        request.extend_from_slice(&[4, 1]);
        request.extend_from_slice(&target.port().to_be_bytes());
        request.extend_from_slice(&ip.octets());
        request.extend_from_slice(username.as_bytes());
        request.push(0);
        stream.write_all(&request).await?;
        let mut response = [0_u8; 8];
        stream.read_exact(&mut response).await?;
        if response[1] == 0x5a {
            return Ok(stream);
        }
        last = Some(io::Error::other(format!(
            "SOCKS4 proxy rejected validated address with status 0x{:02x}",
            response[1]
        )));
    }
    Err(last.unwrap_or_else(|| {
        io::Error::new(
            io::ErrorKind::AddrNotAvailable,
            "SOCKS4 requires a validated IPv4 target",
        )
    }))
}

async fn connect_socks5(
    proxy: &Uri,
    targets: &[SocketAddr],
    credentials: Option<&(String, String)>,
) -> Result<BoxIo, io::Error> {
    let mut last = None;
    for target in targets {
        match connect_socks5_once(proxy, *target, credentials).await {
            Ok(stream) => return Ok(stream),
            Err(error) => last = Some(error),
        }
    }
    Err(last.unwrap_or_else(|| io::Error::other("no validated target address")))
}

async fn connect_socks5_once(
    proxy: &Uri,
    target: SocketAddr,
    credentials: Option<&(String, String)>,
) -> Result<BoxIo, io::Error> {
    let mut stream = connect_proxy_tcp(proxy, 1080).await?;
    let greeting: &[u8] = if credentials.is_some() {
        &[5, 2, 0, 2]
    } else {
        &[5, 1, 0]
    };
    stream.write_all(greeting).await?;
    let mut selected = [0_u8; 2];
    stream.read_exact(&mut selected).await?;
    if selected[0] != 5 || selected[1] == 0xff {
        return Err(io::Error::other(
            "SOCKS5 proxy rejected authentication methods",
        ));
    }
    match selected[1] {
        0 => {}
        2 => {
            let (username, password) =
                credentials.ok_or_else(|| io::Error::other("SOCKS5 proxy requires credentials"))?;
            if username.len() > u8::MAX as usize || password.len() > u8::MAX as usize {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "SOCKS5 credentials exceed protocol limits",
                ));
            }
            let mut auth = Vec::with_capacity(3 + username.len() + password.len());
            auth.extend_from_slice(&[1, username.len() as u8]);
            auth.extend_from_slice(username.as_bytes());
            auth.push(password.len() as u8);
            auth.extend_from_slice(password.as_bytes());
            stream.write_all(&auth).await?;
            let mut result = [0_u8; 2];
            stream.read_exact(&mut result).await?;
            if result != [1, 0] {
                return Err(io::Error::other("SOCKS5 proxy authentication failed"));
            }
        }
        method => {
            return Err(io::Error::other(format!(
                "SOCKS5 proxy selected unsupported authentication method {method}"
            )));
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
    stream.write_all(&request).await?;

    let mut head = [0_u8; 4];
    stream.read_exact(&mut head).await?;
    if head[0] != 5 || head[1] != 0 {
        return Err(io::Error::other(format!(
            "SOCKS5 proxy rejected validated address with status 0x{:02x}",
            head[1]
        )));
    }
    let address_len = match head[3] {
        1 => 4,
        4 => 16,
        3 => {
            let mut length = [0_u8; 1];
            stream.read_exact(&mut length).await?;
            usize::from(length[0])
        }
        _ => {
            return Err(io::Error::other(
                "SOCKS5 proxy returned invalid address type",
            ));
        }
    };
    let mut ignored = vec![0_u8; address_len + 2];
    stream.read_exact(&mut ignored).await?;
    Ok(stream)
}

async fn connect_proxy_tcp(proxy: &Uri, default_port: u16) -> Result<BoxIo, io::Error> {
    let host = proxy
        .host()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "proxy has no host"))?;
    connect_host(host, proxy.port_u16().unwrap_or(default_port)).await
}
