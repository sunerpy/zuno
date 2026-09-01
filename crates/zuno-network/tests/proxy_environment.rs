use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, Instant};

const CHILD_MODE: &str = "ZUNO_NETWORK_PROXY_TEST_CHILD";
const CHILD_URL: &str = "ZUNO_NETWORK_PROXY_TEST_URL";
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
fn lowercase_http_proxy_routes_session_requests() {
    let (proxy, proxy_thread) = serve_once("proxied");
    let output = run_child(
        "proxy",
        "http://proxy-target.invalid/probe",
        &[("http_proxy", format!("http://{proxy}"))],
    );
    proxy_thread.join().expect("proxy server thread");
    assert_child_succeeded(output);
}

#[test]
fn no_proxy_bypasses_an_unreachable_proxy() {
    let (origin, origin_thread) = serve_once("direct");
    let output = run_child(
        "direct",
        &format!("http://{origin}/probe"),
        &[
            ("HTTP_PROXY", "http://127.0.0.1:1".to_owned()),
            ("NO_PROXY", "127.0.0.1".to_owned()),
        ],
    );
    origin_thread.join().expect("origin server thread");
    assert_child_succeeded(output);
}

#[test]
fn direct_policy_ignores_an_ambient_proxy() {
    let (origin, origin_thread) = serve_once("direct-policy");
    let output = run_child(
        "direct-policy",
        &format!("http://{origin}/probe"),
        &[("HTTP_PROXY", "http://127.0.0.1:1".to_owned())],
    );
    origin_thread.join().expect("origin server thread");
    assert_child_succeeded(output);
}

#[test]
fn proxy_probe_child() {
    let Ok(mode) = std::env::var(CHILD_MODE) else {
        return;
    };
    let url = std::env::var(CHILD_URL).expect("child URL");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let body = runtime.block_on(async {
        let client = if mode == "direct-policy" {
            zuno_network::direct_client_builder(zuno_network::DirectPurpose::LoopbackControlPlane)
                .build()
                .expect("direct client")
        } else {
            zuno_network::client()
        };
        client
            .get(url)
            .send()
            .await
            .expect("request succeeds")
            .error_for_status()
            .expect("successful status")
            .text()
            .await
            .expect("response body")
    });
    let expected = match mode.as_str() {
        "proxy" => "proxied",
        "direct" => "direct",
        "direct-policy" => "direct-policy",
        other => panic!("unknown child mode {other}"),
    };
    assert_eq!(body, expected);
}

fn run_child(mode: &str, url: &str, variables: &[(&str, String)]) -> Output {
    let mut command = Command::new(std::env::current_exe().expect("current test executable"));
    command
        .args(["--exact", "proxy_probe_child", "--nocapture"])
        .env(CHILD_MODE, mode)
        .env(CHILD_URL, url)
        .env_remove("REQUEST_METHOD");
    for key in PROXY_KEYS {
        command.env_remove(key);
    }
    for (key, value) in variables {
        command.env(key, value);
    }
    command.output().expect("run proxy probe child")
}

fn assert_child_succeeded(output: Output) {
    assert!(
        output.status.success(),
        "child failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn serve_once(body: &'static str) -> (SocketAddr, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind test HTTP server");
    listener
        .set_nonblocking(true)
        .expect("make test HTTP listener nonblocking");
    let address = listener.local_addr().expect("test HTTP server address");
    let handle = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        let (mut stream, _) = loop {
            match listener.accept() {
                Ok(connection) => break connection,
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        && Instant::now() < deadline =>
                {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("accept one HTTP request: {error}"),
            }
        };
        // Windows propagates a listener's nonblocking mode to accepted
        // sockets. Return this connection to blocking mode before applying the
        // bounded read timeout, otherwise the first read can race the client
        // and fail with WSAEWOULDBLOCK.
        stream
            .set_nonblocking(false)
            .expect("make accepted test HTTP stream blocking");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("bound test request read");
        read_request(&mut stream);
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream
            .write_all(response.as_bytes())
            .expect("write test HTTP response");
    });
    (address, handle)
}

fn read_request(stream: &mut TcpStream) {
    let mut received = Vec::new();
    let mut buffer = [0_u8; 1024];
    while !received.ends_with(b"\r\n\r\n") {
        let count = stream.read(&mut buffer).expect("read HTTP request");
        assert!(count > 0, "client closed before sending request headers");
        received.extend_from_slice(&buffer[..count]);
        assert!(received.len() < 64 * 1024, "request headers are unbounded");
    }
}
