use async_trait::async_trait;
use reqwest::header::HeaderMap;
use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::process::{Command, Output};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use zuno_network::{
    HostResolver, PublicHttpClient, PublicHttpError, PublicHttpPolicy, PublicTarget,
};

const CHILD_MODE: &str = "ZUNO_PUBLIC_PROXY_CHILD";
const CHILD_PROXY: &str = "ZUNO_PUBLIC_PROXY_ADDRESS";
const PROXY_KEYS: [&str; 8] = [
    "HTTP_PROXY",
    "http_proxy",
    "HTTPS_PROXY",
    "https_proxy",
    "ALL_PROXY",
    "all_proxy",
    "NO_PROXY",
    "no_proxy",
];

#[test]
fn http_proxy_receives_the_validated_ip_and_original_host() {
    let (proxy, handle) = http_proxy(false);
    let output = run_child("http", proxy, &[("HTTP_PROXY", format!("http://{proxy}"))]);
    assert_child_succeeded(output);
    let request = handle.join().expect("HTTP proxy thread");
    assert!(
        request.starts_with("GET http://93.184.216.34:8080/probe?x=1 HTTP/1.1\r\n"),
        "{request}"
    );
    assert!(
        request
            .to_ascii_lowercase()
            .contains("\r\nhost: origin.example:8080\r\n"),
        "{request}"
    );
}

#[test]
fn https_target_connects_the_validated_ip_not_the_origin_hostname() {
    let (proxy, handle) = http_proxy(true);
    let output = run_child(
        "https-connect",
        proxy,
        &[("HTTPS_PROXY", format!("http://{proxy}"))],
    );
    assert_child_succeeded(output);
    let request = handle.join().expect("CONNECT proxy thread");
    assert!(
        request.starts_with("CONNECT 93.184.216.34:443 HTTP/1.1\r\n"),
        "{request}"
    );
}

#[test]
fn https_connect_retries_each_locally_validated_target_ip() {
    let (proxy, handle) = refusing_http_proxy(2);
    let output = run_child(
        "https-connect-fallback",
        proxy,
        &[("HTTPS_PROXY", format!("http://{proxy}"))],
    );
    assert_child_succeeded(output);
    let requests = handle.join().expect("CONNECT proxy thread");
    assert_eq!(requests.len(), 2);
    assert!(
        requests[0].starts_with("CONNECT 93.184.216.34:443 HTTP/1.1\r\n"),
        "{}",
        requests[0]
    );
    assert!(
        requests[1].starts_with("CONNECT 93.184.216.35:443 HTTP/1.1\r\n"),
        "{}",
        requests[1]
    );
}

/// A refused credential is decided before the destination address matters.
///
/// The child resolves two validated addresses, so the count is the assertion: opening a
/// second CONNECT to ask the same proxy for the same credential again buys nothing.
#[test]
fn proxy_authentication_failure_is_not_retryable() {
    let (proxy, handle) = connect_rejecting_http_proxy("407 Proxy Authentication Required", 2);
    let output = run_child(
        "connect-auth",
        proxy,
        &[(
            "HTTPS_PROXY",
            format!("http://proxy-user:proxy-secret@{proxy}"),
        )],
    );
    assert_child_succeeded(output);
    let requests = handle.join().expect("CONNECT proxy thread");
    assert_eq!(
        requests.len(),
        1,
        "a refused credential must not be re-asked once per validated address: {requests:?}"
    );
    assert!(
        requests[0].starts_with("CONNECT 93.184.216.34:443 HTTP/1.1\r\n"),
        "{}",
        requests[0]
    );
}

/// A proxy ACL is per destination, so every validated address still gets its turn.
///
/// `403` denies one destination rather than the request, and a proxy that permits one of
/// an origin's addresses while denying another is ordinary. The request as a whole is
/// still permanent once every address has been denied.
#[test]
fn a_denied_destination_does_not_cancel_the_other_validated_addresses() {
    let (proxy, handle) = connect_rejecting_http_proxy("403 Forbidden", 3);
    let output = run_child(
        "connect-denied",
        proxy,
        &[("HTTPS_PROXY", format!("http://{proxy}"))],
    );
    assert_child_succeeded(output);
    let requests = handle.join().expect("CONNECT proxy thread");
    assert_eq!(
        requests.len(),
        2,
        "each validated address must be offered to the proxy once: {requests:?}"
    );
    assert!(
        requests[0].starts_with("CONNECT 93.184.216.34:443 HTTP/1.1\r\n"),
        "{}",
        requests[0]
    );
    assert!(
        requests[1].starts_with("CONNECT 93.184.216.35:443 HTTP/1.1\r\n"),
        "{}",
        requests[1]
    );
}

/// A denied destination stays denied when another address merely stalls.
///
/// The proxy answers `403` to the first validated address and then accepts the second
/// CONNECT without ever replying, so the second address can only end in the per-address
/// timeout. That is the exact mixed answer a hostile DNS response produces: one
/// routable-but-denied address plus one that goes silent. The request must surface the
/// denial and must not be retried on backoff.
#[test]
fn a_denied_destination_is_not_laundered_by_a_stalled_sibling() {
    let (proxy, handle) = connect_denying_then_stalling_http_proxy(4);
    let output = run_child(
        "connect-denied-stall",
        proxy,
        &[("HTTPS_PROXY", format!("http://{proxy}"))],
    );
    assert_child_succeeded(output);
    let requests = handle.join().expect("CONNECT proxy thread");
    assert_eq!(
        requests.len(),
        2,
        "the denial must not cancel the second validated address: {requests:?}"
    );
    assert!(
        requests[0].starts_with("CONNECT 93.184.216.34:443 HTTP/1.1\r\n"),
        "{}",
        requests[0]
    );
    assert!(
        requests[1].starts_with("CONNECT 93.184.216.35:443 HTTP/1.1\r\n"),
        "{}",
        requests[1]
    );
}

/// A peer-supplied address count must not multiply CONNECT round trips.
///
/// The resolver answers with 16 validated addresses and the proxy denies every one. The
/// count is the assertion: one `webfetch` call must not turn one DNS answer into sixteen
/// proxy connections.
#[test]
fn a_large_validated_answer_caps_the_proxy_round_trips() {
    let (proxy, handle) = connect_rejecting_http_proxy("403 Forbidden", 12);
    let output = run_child(
        "connect-denied-many",
        proxy,
        &[("HTTPS_PROXY", format!("http://{proxy}"))],
    );
    assert_child_succeeded(output);
    let requests = handle.join().expect("CONNECT proxy thread");
    assert_eq!(
        requests.len(),
        8,
        "the address walk is capped at 8 attempts: {requests:?}"
    );
}

#[test]
fn socks5_credential_rejection_is_not_retryable() {
    let (proxy, handle) = socks5_auth_rejecting_proxy(2);
    let output = run_child(
        "socks5-auth",
        proxy,
        &[(
            "ALL_PROXY",
            format!("socks5h://proxy-user:proxy-secret@{proxy}"),
        )],
    );
    assert_child_succeeded(output);
    let exchanges = handle.join().expect("SOCKS5 proxy thread");
    assert_eq!(
        exchanges.len(),
        1,
        "a rejected username/password is not a property of the destination: {exchanges:?}"
    );
    assert_eq!(exchanges[0], b"proxy-user");
}

#[test]
fn a_cgi_style_request_method_disables_the_proxy_environment() {
    let proxy = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 1);
    let output = run_child(
        "cgi",
        proxy,
        &[
            ("HTTP_PROXY", format!("http://{proxy}")),
            ("REQUEST_METHOD", "GET".to_owned()),
        ],
    );
    assert_child_succeeded(output);
}

#[test]
fn socks5h_is_forced_to_the_locally_validated_ip() {
    let (proxy, handle) = socks5_proxy();
    let output = run_child(
        "socks5",
        proxy,
        &[("ALL_PROXY", format!("socks5h://{proxy}"))],
    );
    assert_child_succeeded(output);
    let observed = handle.join().expect("SOCKS5 proxy thread");
    assert_eq!(
        observed.target,
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34)), 8080)
    );
    assert!(
        observed
            .request
            .to_ascii_lowercase()
            .contains("\r\nhost: origin.example:8080\r\n"),
        "{}",
        observed.request
    );
}

#[test]
fn no_proxy_is_the_only_environment_selected_direct_route() {
    let proxy = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 1);
    let output = run_child(
        "no-proxy",
        proxy,
        &[
            ("HTTP_PROXY", format!("http://{proxy}")),
            ("NO_PROXY", "origin.example".to_owned()),
        ],
    );
    assert_child_succeeded(output);
}

#[test]
fn https_proxy_scheme_is_selected_without_exposing_credentials() {
    let proxy = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 1);
    let output = run_child(
        "https-route",
        proxy,
        &[(
            "HTTPS_PROXY",
            "https://proxy-user:proxy-secret@proxy.example:8443".to_owned(),
        )],
    );
    assert_child_succeeded(output);
}

#[test]
fn proxy_failures_do_not_render_proxy_credentials() {
    let proxy = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 1);
    let output = run_child(
        "redaction",
        proxy,
        &[(
            "HTTP_PROXY",
            format!("http://proxy-user:proxy-secret@{proxy}"),
        )],
    );
    assert_child_succeeded(output);
}

#[test]
fn public_proxy_child() {
    let Ok(mode) = std::env::var(CHILD_MODE) else {
        return;
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    runtime.block_on(async move {
        // Every mode whose assertion is about the per-address loop resolves two
        // addresses; a single-address resolver cannot tell a short-circuit from a
        // per-destination retry.
        let resolver: Arc<dyn HostResolver> = if mode == "connect-denied-many" {
            Arc::new(ManyResolver)
        } else if matches!(
            mode.as_str(),
            "https-connect-fallback"
                | "connect-auth"
                | "connect-denied"
                | "connect-denied-stall"
                | "socks5-auth"
        ) {
            Arc::new(MultiResolver)
        } else {
            Arc::new(FixedResolver)
        };
        let client = PublicHttpClient::with_resolver(resolver, PublicHttpPolicy::default());
        // A stalled address is only observable inside a test's own patience, so the modes
        // that depend on one tighten the per-address establishment budget through the
        // public setter. Every other mode keeps the shipped default.
        let client = if mode == "connect-denied-stall" {
            client.with_establish_timeout(Duration::from_millis(300))
        } else {
            client
        };
        let target = if matches!(
            mode.as_str(),
            "https-connect"
                | "https-connect-fallback"
                | "https-route"
                | "connect-auth"
                | "connect-denied"
                | "connect-denied-stall"
                | "connect-denied-many"
        ) {
            PublicTarget::parse("https://origin.example/probe").expect("HTTPS target")
        } else {
            PublicTarget::parse("http://origin.example:8080/probe?x=1").expect("HTTP target")
        };
        match mode.as_str() {
            "http" | "socks5" => {
                let mut response = client
                    .get(target, HeaderMap::new())
                    .await
                    .expect("proxied public request");
                assert_eq!(
                    response.route(),
                    if mode == "http" {
                        "http_proxy"
                    } else {
                        "socks5_proxy"
                    }
                );
                let mut body = Vec::new();
                while let Some(chunk) = response.chunk().await.expect("response chunk") {
                    body.extend_from_slice(&chunk);
                }
                assert_eq!(body, b"proxied");
            }
            "https-connect" | "https-connect-fallback" => {
                let error = client
                    .get(target, HeaderMap::new())
                    .await
                    .expect_err("test proxy deliberately refuses CONNECT");
                assert!(matches!(
                    error,
                    PublicHttpError::Transport {
                        route: "http_proxy",
                        ..
                    }
                ));
            }
            "connect-auth"
            | "connect-denied"
            | "connect-denied-stall"
            | "connect-denied-many"
            | "socks5-auth" => {
                let error = client
                    .get(target, HeaderMap::new())
                    .await
                    .expect_err("the test proxy rejects the credentials");
                let expected_route = if mode == "socks5-auth" {
                    "socks5_proxy"
                } else {
                    "http_proxy"
                };
                match &error {
                    PublicHttpError::PermanentTransport { route, .. } => {
                        assert_eq!(*route, expected_route);
                    }
                    other => panic!("expected a permanent transport failure, got {other:?}"),
                }
                assert!(
                    !error.is_transient(),
                    "a refused proxy credential must never be retried: {error}"
                );
                let rendered = format!("{error}\n{error:?}");
                assert!(!rendered.contains("proxy-secret"), "{rendered}");
            }
            "cgi" => {
                assert_eq!(
                    client.route_label(&target).expect("CGI route"),
                    "direct",
                    "REQUEST_METHOD must disable the inherited proxy environment"
                );
            }
            "no-proxy" => {
                assert_eq!(
                    client.route_label(&target).expect("NO_PROXY route"),
                    "no_proxy"
                );
            }
            "https-route" => {
                assert_eq!(
                    client.route_label(&target).expect("HTTPS proxy route"),
                    "https_proxy"
                );
            }
            "redaction" => {
                let error = client
                    .get(target, HeaderMap::new())
                    .await
                    .expect_err("test proxy is deliberately unreachable");
                assert!(matches!(
                    error,
                    PublicHttpError::Transport {
                        route: "http_proxy",
                        ..
                    }
                ));
                let rendered = format!("{error}\n{error:?}");
                assert!(!rendered.contains("proxy-user"), "{rendered}");
                assert!(!rendered.contains("proxy-secret"), "{rendered}");
            }
            other => panic!("unknown child mode {other}"),
        }
    });
}

struct FixedResolver;

#[async_trait]
impl HostResolver for FixedResolver {
    async fn resolve(&self, host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
        if host == "ipv4only.arpa" {
            return Ok(Vec::new());
        }
        assert_eq!(host, "origin.example");
        Ok(vec![SocketAddr::new(
            IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34)),
            port,
        )])
    }
}

struct MultiResolver;

#[async_trait]
impl HostResolver for MultiResolver {
    async fn resolve(&self, host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
        if host == "ipv4only.arpa" {
            return Ok(Vec::new());
        }
        assert_eq!(host, "origin.example");
        Ok(vec![
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34)), port),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(93, 184, 216, 35)), port),
        ])
    }
}

/// A peer answer large enough to make the attempt cap observable.
struct ManyResolver;

#[async_trait]
impl HostResolver for ManyResolver {
    async fn resolve(&self, host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
        if host == "ipv4only.arpa" {
            return Ok(Vec::new());
        }
        assert_eq!(host, "origin.example");
        Ok((34..50)
            .map(|host| SocketAddr::new(IpAddr::V4(Ipv4Addr::new(93, 184, 216, host)), port))
            .collect())
    }
}

fn run_child(mode: &str, proxy: SocketAddr, variables: &[(&str, String)]) -> Output {
    let mut command = Command::new(std::env::current_exe().expect("test executable"));
    command
        .args(["--exact", "public_proxy_child", "--nocapture"])
        .env(CHILD_MODE, mode)
        .env(CHILD_PROXY, proxy.to_string())
        .env_remove("REQUEST_METHOD");
    for key in PROXY_KEYS {
        command.env_remove(key);
    }
    for (key, value) in variables {
        command.env(key, value);
    }
    command.output().expect("run public proxy child")
}

fn assert_child_succeeded(output: Output) {
    assert!(
        output.status.success(),
        "child failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn http_proxy(refuse_connect: bool) -> (SocketAddr, thread::JoinHandle<String>) {
    let listener = listener();
    let address = listener.local_addr().expect("proxy address");
    let handle = thread::spawn(move || {
        let (mut stream, _) = accept(&listener);
        let request = read_request(&mut stream);
        if refuse_connect {
            stream
                .write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n")
                .expect("CONNECT refusal");
        } else {
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\nConnection: close\r\n\r\nproxied",
                )
                .expect("proxy response");
        }
        request
    });
    (address, handle)
}

/// A CONNECT-refusing proxy that keeps accepting until the client stops trying.
///
/// `limit` is deliberately larger than the expected number of attempts so an extra
/// attempt is observed rather than hidden by a fixture that only accepts once.
fn connect_rejecting_http_proxy(
    status: &'static str,
    limit: usize,
) -> (SocketAddr, thread::JoinHandle<Vec<String>>) {
    let listener = listener();
    let address = listener.local_addr().expect("proxy address");
    let handle = thread::spawn(move || {
        let mut requests = Vec::new();
        while requests.len() < limit {
            let Some(mut stream) = try_accept(&listener) else {
                break;
            };
            requests.push(read_request(&mut stream));
            stream
                .write_all(format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\n\r\n").as_bytes())
                .expect("CONNECT rejection");
        }
        requests
    });
    (address, handle)
}

/// Deny the first CONNECT, then accept and never answer.
///
/// Holding the accepted stream is what makes the later attempts stall: no reply and no
/// FIN, which is exactly what a silent address looks like to the transport. Accepting
/// stays fast, so the recorded request count is the client's, not the fixture's.
fn connect_denying_then_stalling_http_proxy(
    limit: usize,
) -> (SocketAddr, thread::JoinHandle<Vec<String>>) {
    let listener = listener();
    let address = listener.local_addr().expect("proxy address");
    let handle = thread::spawn(move || {
        let mut requests = Vec::new();
        let mut held = Vec::new();
        while requests.len() < limit {
            let Some(mut stream) = try_accept(&listener) else {
                break;
            };
            requests.push(read_request(&mut stream));
            if requests.len() == 1 {
                stream
                    .write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n")
                    .expect("CONNECT denial");
            } else {
                held.push(stream);
            }
        }
        requests
    });
    (address, handle)
}

fn socks5_auth_rejecting_proxy(limit: usize) -> (SocketAddr, thread::JoinHandle<Vec<Vec<u8>>>) {
    let listener = listener();
    let address = listener.local_addr().expect("proxy address");
    let handle = thread::spawn(move || {
        let mut exchanges = Vec::new();
        while exchanges.len() < limit {
            let Some(mut stream) = try_accept(&listener) else {
                break;
            };
            exchanges.push(socks5_reject_credentials(&mut stream));
        }
        exchanges
    });
    (address, handle)
}

fn socks5_reject_credentials(stream: &mut TcpStream) -> Vec<u8> {
    let mut greeting = [0_u8; 4];
    stream.read_exact(&mut greeting).expect("SOCKS greeting");
    assert_eq!(greeting, [5, 2, 0, 2]);
    stream
        .write_all(&[5, 2])
        .expect("SOCKS username/password method");
    let mut header = [0_u8; 2];
    stream.read_exact(&mut header).expect("SOCKS auth header");
    assert_eq!(header[0], 1);
    let mut username = vec![0_u8; usize::from(header[1])];
    stream.read_exact(&mut username).expect("SOCKS username");
    let mut password_length = [0_u8; 1];
    stream
        .read_exact(&mut password_length)
        .expect("SOCKS password length");
    let mut password = vec![0_u8; usize::from(password_length[0])];
    stream.read_exact(&mut password).expect("SOCKS password");
    stream.write_all(&[1, 1]).expect("SOCKS auth rejection");
    username
}

fn refusing_http_proxy(count: usize) -> (SocketAddr, thread::JoinHandle<Vec<String>>) {
    let listener = listener();
    let address = listener.local_addr().expect("proxy address");
    let handle = thread::spawn(move || {
        (0..count)
            .map(|_| {
                let (mut stream, _) = accept(&listener);
                let request = read_request(&mut stream);
                stream
                    .write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n")
                    .expect("CONNECT refusal");
                request
            })
            .collect()
    });
    (address, handle)
}

struct SocksObservation {
    target: SocketAddr,
    request: String,
}

fn socks5_proxy() -> (SocketAddr, thread::JoinHandle<SocksObservation>) {
    let listener = listener();
    let address = listener.local_addr().expect("proxy address");
    let handle = thread::spawn(move || {
        let (mut stream, _) = accept(&listener);
        let mut greeting = [0_u8; 3];
        stream.read_exact(&mut greeting).expect("SOCKS greeting");
        assert_eq!(greeting, [5, 1, 0]);
        stream.write_all(&[5, 0]).expect("SOCKS method");

        let mut request = [0_u8; 10];
        stream.read_exact(&mut request).expect("SOCKS connect");
        assert_eq!(&request[..4], &[5, 1, 0, 1]);
        let target = SocketAddr::new(
            IpAddr::V4(Ipv4Addr::new(
                request[4], request[5], request[6], request[7],
            )),
            u16::from_be_bytes([request[8], request[9]]),
        );
        stream
            .write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 1])
            .expect("SOCKS success");
        let request = read_request(&mut stream);
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\nConnection: close\r\n\r\nproxied")
            .expect("origin response through SOCKS");
        SocksObservation { target, request }
    });
    (address, handle)
}

fn listener() -> TcpListener {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind proxy");
    listener
        .set_nonblocking(true)
        .expect("nonblocking proxy listener");
    listener
}

/// Accept one connection, or report that the client stopped trying.
///
/// The grace period only has to outlast one more connection attempt from a child that has
/// already made its first, so a short wait keeps the "exactly one attempt" assertions
/// cheap while still observing a second attempt if the transport makes one.
fn try_accept(listener: &TcpListener) -> Option<TcpStream> {
    let deadline = Instant::now() + Duration::from_millis(750);
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream
                    .set_nonblocking(false)
                    .expect("blocking proxy stream");
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .expect("proxy read timeout");
                return Some(stream);
            }
            Err(error)
                if error.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline =>
            {
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return None,
            Err(error) => panic!("accept proxy connection: {error}"),
        }
    }
}

fn accept(listener: &TcpListener) -> (TcpStream, SocketAddr) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match listener.accept() {
            Ok((stream, peer)) => {
                stream
                    .set_nonblocking(false)
                    .expect("blocking proxy stream");
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .expect("proxy read timeout");
                return (stream, peer);
            }
            Err(error)
                if error.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline =>
            {
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("accept proxy connection: {error}"),
        }
    }
}

fn read_request(stream: &mut TcpStream) -> String {
    let mut received = Vec::new();
    let mut buffer = [0_u8; 1024];
    while !received.ends_with(b"\r\n\r\n") {
        let count = stream.read(&mut buffer).expect("read request");
        assert!(count > 0, "client closed before request headers");
        received.extend_from_slice(&buffer[..count]);
        assert!(received.len() < 64 * 1024, "request headers are bounded");
    }
    String::from_utf8(received).expect("HTTP request text")
}
