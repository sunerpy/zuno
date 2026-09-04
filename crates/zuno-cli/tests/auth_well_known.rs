//! `zuno auth login <url>` runs a program that the remote host names.
//!
//! The `/.well-known/zuno` document tells Zuno which program to run to obtain a
//! credential, and that program runs with the user's privileges. These tests pin
//! the two halves of the boundary around it: without a terminal to confirm, and
//! without the explicit `--trust-remote-command` opt-in, nothing is fetched, nothing
//! is spawned, and nothing is stored; with the opt-in, the command is shown before it
//! runs and a credential is stored only after it exits successfully.
//!
//! The fixture is a real local HTTP server so the refusal is measured against a
//! document that would have produced a spawn, not against a network failure.

use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

/// A local `/.well-known/zuno` server that counts the requests it answered.
struct WellKnownFixture {
    base_url: String,
    hits: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl WellKnownFixture {
    fn serve(document: serde_json::Value) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback fixture");
        listener
            .set_nonblocking(true)
            .expect("poll the fixture listener");
        let port = listener.local_addr().expect("fixture address").port();
        let body = serde_json::to_vec(&document).expect("serialize well-known document");
        let hits = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let hits = Arc::clone(&hits);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            stream
                                .set_nonblocking(false)
                                .expect("blocking fixture stream");
                            stream
                                .set_read_timeout(Some(Duration::from_secs(2)))
                                .expect("fixture read timeout");
                            let mut request = Vec::new();
                            let mut buffer = [0_u8; 1024];
                            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                                match stream.read(&mut buffer) {
                                    Ok(0) | Err(_) => break,
                                    Ok(read) => request.extend_from_slice(&buffer[..read]),
                                }
                            }
                            hits.fetch_add(1, Ordering::SeqCst);
                            let head = format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                                 Content-Length: {}\r\nConnection: close\r\n\r\n",
                                body.len()
                            );
                            let _ = stream.write_all(head.as_bytes());
                            let _ = stream.write_all(&body);
                            let _ = stream.flush();
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
            })
        };
        Self {
            base_url: format!("http://127.0.0.1:{port}"),
            hits,
            stop,
            thread: Some(thread),
        }
    }

    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }
}

impl Drop for WellKnownFixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn well_known_document(command: serde_json::Value) -> serde_json::Value {
    serde_json::json!({"auth": {"command": command, "env": "ACME_TOKEN"}})
}

/// Split a well-known `command` array into the program and its arguments, the way the
/// login does, so a test can check that exactly what was served is what was shown.
fn argv(command: &serde_json::Value) -> (&str, Vec<&str>) {
    let elements: Vec<&str> = command
        .as_array()
        .expect("the command is an array")
        .iter()
        .map(|element| element.as_str().expect("each command element is a string"))
        .collect();
    let (program, arguments) = elements.split_first().expect("the command names a program");
    (program, arguments.to_vec())
}

struct Sandbox {
    root: tempfile::TempDir,
}

impl Sandbox {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("temporary login environment");
        for directory in ["home", "data", "config", "cache", "state"] {
            std::fs::create_dir_all(root.path().join(directory)).expect("isolated directory");
        }
        Self { root }
    }

    fn marker(&self) -> PathBuf {
        self.root.path().join("remote-command-ran")
    }

    /// The remote-chosen command: it leaves the marker file when it runs, prints a
    /// token, and exits with `exit_code`. A code other than zero makes it fail after
    /// leaving the marker, which is how "it ran but the credential must not be stored"
    /// is told apart from "it never ran".
    ///
    /// On Unix the command is an inline `sh -c` script. On Windows it is a `.cmd` script
    /// written into the sandbox and named by path, because `cmd /C` takes the rest of its
    /// command line as text rather than as argv: the `\"` that a CRT-style spawn (Rust's
    /// `Command`, like Node's or Python's) uses to carry an inner quote is not an escape
    /// to cmd.exe, so an inline command that quotes the marker path never runs as
    /// written. A script path is the one argument cmd.exe keeps intact even when it
    /// contains spaces, and it is what a real document targeting Windows would name.
    /// `/D` keeps a developer host's AutoRun commands from writing into the token.
    fn remote_command(&self, exit_code: i32) -> serde_json::Value {
        let marker = self.marker();
        let marker = marker.display();
        #[cfg(windows)]
        {
            let script = self.root.path().join("remote-command.cmd");
            std::fs::write(
                &script,
                format!(
                    "@echo off\r\necho spawned > \"{marker}\"\r\necho TOKEN\r\nexit {exit_code}\r\n"
                ),
            )
            .expect("write the remote command script");
            serde_json::json!(["cmd", "/D", "/C", script.display().to_string()])
        }
        #[cfg(not(windows))]
        {
            serde_json::json!([
                "sh",
                "-c",
                format!("echo spawned > '{marker}' && echo TOKEN && exit {exit_code}")
            ])
        }
    }

    fn auth_json(&self) -> PathBuf {
        self.root.path().join("data/zuno/auth.json")
    }

    /// Run `zuno auth login` with every stream redirected, so stdin is not a terminal.
    fn login(&self, args: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_zuno"));
        command
            .arg("auth")
            .arg("login")
            .args(args)
            .current_dir(self.root.path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("NO_COLOR", "1")
            .env("TERM", "dumb")
            .env("HOME", self.root.path().join("home"))
            .env("XDG_DATA_HOME", self.root.path().join("data"))
            .env("XDG_CONFIG_HOME", self.root.path().join("config"))
            .env("XDG_CACHE_HOME", self.root.path().join("cache"))
            .env("XDG_STATE_HOME", self.root.path().join("state"))
            .env("ZUNO_DISABLE_PROJECT_CONFIG", "1")
            .env("ZUNO_DISABLE_AUTOUPDATE", "true")
            .env("ZUNO_DISABLE_MODELS_FETCH", "true")
            .env("ZUNO_DISABLE_LSP_DOWNLOAD", "true");
        for proxy in [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
        ] {
            command.env_remove(proxy);
        }
        command.output().expect("run zuno auth login")
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn a_url_login_without_a_terminal_or_the_trust_flag_fetches_spawns_and_stores_nothing() {
    let sandbox = Sandbox::new();
    let fixture = WellKnownFixture::serve(well_known_document(sandbox.remote_command(0)));

    let output = sandbox.login(&[fixture.base_url.as_str()]);

    let stderr = stderr(&output);
    assert!(!output.status.success(), "{stderr}");
    assert!(
        stderr.contains("requires an interactive terminal to confirm"),
        "{stderr}"
    );
    assert!(stderr.contains("--trust-remote-command"), "{stderr}");
    assert!(
        stderr.contains("runs a program chosen by"),
        "the refusal must say why: {stderr}"
    );
    assert!(
        !sandbox.marker().exists(),
        "the remote-chosen command was spawned without confirmation"
    );
    assert!(
        !sandbox.auth_json().exists(),
        "a credential was stored without the command having been confirmed"
    );
    assert_eq!(
        fixture.hits(),
        0,
        "the well-known document was fetched even though nothing could be confirmed"
    );
}

#[test]
fn the_trust_flag_shows_the_remote_command_runs_it_and_stores_the_credential_after_success() {
    let sandbox = Sandbox::new();
    let command = sandbox.remote_command(0);
    let fixture = WellKnownFixture::serve(well_known_document(command.clone()));

    let output = sandbox.login(&[fixture.base_url.as_str(), "--trust-remote-command"]);

    let stderr = stderr(&output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "stdout: {stdout}\nstderr: {stderr}"
    );
    let (program, arguments) = argv(&command);
    assert!(
        stderr.contains(&format!("program: {program:?}")),
        "the program must be shown before it runs: {stderr}"
    );
    assert!(
        stderr.contains(&format!("arguments ({}):", arguments.len())),
        "the argument count must be shown: {stderr}"
    );
    for argument in &arguments {
        assert!(
            stderr.contains(&format!("{argument:?}")),
            "every argument must be shown as served, {argument:?} was not: {stderr}"
        );
    }
    assert!(stderr.contains("the remote host did"), "{stderr}");
    assert!(
        stderr.contains("without confirmation because --trust-remote-command"),
        "{stderr}"
    );
    assert!(sandbox.marker().exists(), "the trusted command did not run");
    assert!(
        stdout.contains(&format!("Logged into {}", fixture.base_url)),
        "{stdout}"
    );
    assert_eq!(fixture.hits(), 1);

    let auth: serde_json::Value = serde_json::from_slice(
        &std::fs::read(sandbox.auth_json()).expect("the credential file exists"),
    )
    .expect("parse stored credentials");
    let stored = &auth[fixture.base_url.as_str()];
    assert_eq!(stored["type"], "wellknown", "{auth}");
    assert_eq!(stored["key"], "ACME_TOKEN", "{auth}");
    assert_eq!(stored["token"], "TOKEN", "{auth}");
}

#[test]
fn a_trusted_remote_command_that_fails_stores_no_credential() {
    let sandbox = Sandbox::new();
    let fixture = WellKnownFixture::serve(well_known_document(sandbox.remote_command(3)));

    let output = sandbox.login(&[fixture.base_url.as_str(), "--trust-remote-command"]);

    let stderr = stderr(&output);
    assert!(!output.status.success(), "{stderr}");
    assert!(
        sandbox.marker().exists(),
        "the command was trusted, so it must have run"
    );
    assert!(stderr.contains("no credential was stored"), "{stderr}");
    assert!(
        !sandbox.auth_json().exists(),
        "a credential was stored although the command failed"
    );
}

#[test]
fn a_hostname_that_starts_with_the_loopback_spelling_is_refused_before_any_fetch() {
    let sandbox = Sandbox::new();
    for url in [
        "http://127.0.0.1.attacker.example",
        "http://user@127.0.0.1@evil/",
        "http://127.0.0.1@attacker.example/.well-known/zuno",
    ] {
        let output = sandbox.login(&[url, "--trust-remote-command"]);
        let stderr = stderr(&output);
        assert!(!output.status.success(), "{url}: {stderr}");
        assert!(
            stderr.contains("loopback IP address"),
            "{url} must be refused by the transport guard: {stderr}"
        );
        assert!(
            !stderr.contains("Failed to load auth provider metadata"),
            "{url} must be refused before any request is made: {stderr}"
        );
    }
    assert!(!sandbox.auth_json().exists());
}

#[test]
fn the_trust_flag_is_refused_for_anything_but_a_url_login() {
    let sandbox = Sandbox::new();
    for args in [
        &["openai", "--trust-remote-command"][..],
        &["--provider", "openai", "--trust-remote-command"][..],
        &["--trust-remote-command"][..],
    ] {
        let output = sandbox.login(args);
        let stderr = stderr(&output);
        assert!(!output.status.success(), "{args:?}: {stderr}");
        assert!(
            stderr.contains("applies only to a URL login")
                || stderr.contains("required arguments were not provided"),
            "{args:?}: {stderr}"
        );
    }
    assert!(!sandbox.auth_json().exists());
}
