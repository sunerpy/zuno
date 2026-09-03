//! `server.port` reaches the bound socket, through the production `serve` handler.
//!
//! The unit tests beside `listen_config` prove the precedence rules. They cannot prove
//! that `serve` still calls it: a revert to the old `ServerConfig::default()
//! .with_hostname(&args.hostname).with_port(args.port)` composition would leave those
//! tests green while the configured port went back to being read by nobody. So this
//! runs the shipped binary against a `zuno.json` whose `server.port` is the only source
//! of the number, and reads the port out of the readiness line the user sees.

use std::io::{BufRead as _, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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

/// Collect a stream's lines on its own thread.
///
/// Both streams need a reader for the whole life of the child. `wait_with_output` would
/// read them for us, but it returns only once every writer has closed its end, and on
/// Windows that is not the same as the child having exited — see [`terminate_tree`]. A
/// stream nobody reads is worse still: the child blocks forever on a full pipe.
fn collect(stream: impl std::io::Read + Send + 'static, sink: mpsc::Sender<String>) {
    std::thread::spawn(move || {
        for line in BufReader::new(stream).lines().map_while(Result::ok) {
            if sink.send(line).is_err() {
                break;
            }
        }
    });
}

/// End the served tree, not only the process this fixture spawned.
///
/// `run_process` restarts the executable once with its bootstrap environment. On Unix
/// that restart is an `exec`, so the child this fixture holds *is* the server. Windows
/// has no `exec`, so the restart is a real child and the process this fixture holds is
/// only its waiter: killing the waiter alone leaves the server running, still owning
/// the inherited pipes, and the test would then hang instead of fail. `taskkill /T`
/// ends the waiter and everything below it.
fn terminate_tree(child: &mut Child) {
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/T", "/F", "/PID", &child.id().to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
    let _ = child.wait();
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
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the shipped binary must start");

    let (sender, receiver) = mpsc::channel();
    collect(child.stdout.take().expect("piped stdout"), sender.clone());
    let stderr_lines = Arc::new(Mutex::new(Vec::new()));
    let (stderr_sender, stderr_receiver) = mpsc::channel();
    collect(child.stderr.take().expect("piped stderr"), stderr_sender);
    {
        let stderr_lines = Arc::clone(&stderr_lines);
        std::thread::spawn(move || {
            for line in stderr_receiver {
                stderr_lines.lock().expect("stderr transcript").push(line);
            }
        });
    }

    // One deadline for the whole wait, not one per line: a binary that streams
    // anything at all must not be able to hold the suite open indefinitely.
    let deadline = Instant::now() + READINESS_TIMEOUT;
    let mut transcript = Vec::new();
    let readiness = loop {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            break None;
        };
        match receiver.recv_timeout(remaining) {
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

    terminate_tree(&mut child);
    let stderr = stderr_lines.lock().expect("stderr transcript").join("\n");

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
