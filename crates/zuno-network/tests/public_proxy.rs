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
        let resolver: Arc<dyn HostResolver> = if mode == "https-connect-fallback" {
            Arc::new(MultiResolver)
        } else {
            Arc::new(FixedResolver)
        };
        let client = PublicHttpClient::with_resolver(resolver, PublicHttpPolicy::default());
        let target = if matches!(
            mode.as_str(),
            "https-connect" | "https-connect-fallback" | "https-route"
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

fn accept(listener: &TcpListener) -> (TcpStream, SocketAddr) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match listener.accept() {
            Ok((stream, peer)) => {
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
