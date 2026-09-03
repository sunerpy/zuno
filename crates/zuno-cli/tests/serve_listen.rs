//! `server.port` reaches the bound socket, through the production `serve` handler.
//!
//! The unit tests beside `listen_config` prove the precedence rules. They cannot prove
//! that `serve` still calls it: a revert to the old `ServerConfig::default()
//! .with_hostname(&args.hostname).with_port(args.port)` composition would leave those
//! tests green while the configured port went back to being read by nobody. So this
//! runs the shipped binary against a `zuno.json` whose `server.port` is the only source
//! of the number, and reads the port out of the readiness line the user sees.

use std::io::{BufRead as _, BufReader};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

/// Long enough for a cold binary to open its database and bind, short enough that a
/// regression fails the run instead of hanging it.
const READINESS_TIMEOUT: Duration = Duration::from_secs(90);

/// A port nothing is listening on, obtained by binding and releasing it.
fn free_port() -> u16 {
    let listener =
        std::net::TcpListener::bind("127.0.0.1:0").expect("a loopback port for the fixture");
    listener
        .local_addr()
        .expect("the bound fixture address")
        .port()
}

#[test]
fn serve_binds_the_port_configured_in_zuno_json() {
    let root = tempfile::tempdir().expect("private serve root");
    let port = free_port();
    std::fs::write(
        root.path().join("zuno.json"),
        format!("{{\n  \"server\": {{ \"port\": {port} }}\n}}\n"),
    )
    .expect("project configuration");

    // The isolation mirrors `tests/surface.rs`: no developer config, credentials,
    // sessions or database, and no network fetches during startup.
    let mut child = Command::new(env!("CARGO_BIN_EXE_zuno"))
        .arg("serve")
        .current_dir(root.path())
        .env("NO_COLOR", "1")
        .env("TERM", "dumb")
        .env("HOME", root.path().join("home"))
        .env("XDG_DATA_HOME", root.path().join("data"))
        .env("XDG_CONFIG_HOME", root.path().join("config"))
        .env("XDG_CACHE_HOME", root.path().join("cache"))
        .env("XDG_STATE_HOME", root.path().join("state"))
        .env("ZUNO_DB", root.path().join("opencode.db"))
        .env("ZUNO_DISABLE_AUTOUPDATE", "true")
        .env("ZUNO_DISABLE_MODELS_FETCH", "true")
        .env("ZUNO_DISABLE_LSP_DOWNLOAD", "true")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the shipped binary must start");

    let stdout = child.stdout.take().expect("piped stdout");
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if sender.send(line).is_err() {
                break;
            }
        }
    });

    let mut transcript = Vec::new();
    let readiness = loop {
        match receiver.recv_timeout(READINESS_TIMEOUT) {
            Ok(line) => {
                let matched = line.contains("listening on");
                transcript.push(line.clone());
                if matched {
                    break Some(line);
                }
            }
            Err(_) => break None,
        }
    };

    let _ = child.kill();
    let output = child.wait_with_output().expect("the server must be reaped");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    let readiness = readiness.unwrap_or_else(|| {
        panic!(
            "`serve` never reported a listening address; stdout: {transcript:?}; stderr: {stderr}"
        )
    });
    assert!(
        readiness.ends_with(&format!("127.0.0.1:{port}")),
        "`server.port` did not reach the bound socket; readiness line: {readiness}"
    );
}
