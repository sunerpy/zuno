#![cfg(unix)]

use std::fs;
use std::io::{Read as _, Write};
use std::os::unix::fs::PermissionsExt as _;
use std::process::Command;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

#[test]
fn bare_auth_login_selects_a_provider_and_stores_a_hidden_api_key() {
    let root = tempfile::tempdir().expect("temporary login environment");
    let models = root.path().join("models.json");
    fs::write(
        &models,
        r#"{
          "acme": {
            "id": "acme",
            "name": "Acme",
            "api": "https://acme.example.test/v1",
            "npm": "@ai-sdk/openai-compatible",
            "env": ["ACME_API_KEY"],
            "models": {
              "acme-test": {
                "id": "acme-test",
                "name": "Acme Test"
              }
            }
          }
        }"#,
    )
    .expect("write provider catalog");

    let data = root.path().join("data");
    let config = root.path().join("config");
    let cache = root.path().join("cache");
    let home = root.path().join("home");
    for directory in [&data, &config, &cache, &home] {
        fs::create_dir_all(directory).expect("create isolated directory");
    }
    let zuno_config = config.join("zuno");
    fs::create_dir_all(&zuno_config).expect("create Zuno config directory");
    fs::write(
        zuno_config.join("zuno.json"),
        r#"{
          "provider": {
            "acme": {
              "name": "Acme",
              "transport": "openai-compatible",
              "options": {"baseURL": "https://acme.example.test/v1"},
              "models": {"acme-test": {"name": "Acme Test"}}
            }
          }
        }"#,
    )
    .expect("write configured provider");

    let mut terminal = TestPty::spawn(
        root.path(),
        &[
            ("HOME", home.as_path()),
            ("XDG_DATA_HOME", data.as_path()),
            ("XDG_CONFIG_HOME", config.as_path()),
            ("XDG_CACHE_HOME", cache.as_path()),
            ("ZUNO_MODELS_PATH", models.as_path()),
        ],
    );
    assert!(
        terminal.wait_for_output("Select provider"),
        "{}",
        terminal.output()
    );
    terminal.write(b"acme\r");
    assert!(
        terminal.wait_for_output("Enter API key"),
        "{}",
        terminal.output()
    );
    terminal.write(b"super-secret-login-key\r");

    let (status, output) = terminal.finish_after_output("Stored API key for acme");
    assert!(status.success(), "{output}");
    assert!(output.contains("Select provider: Acme"), "{output}");
    assert!(output.contains("Stored API key for acme"), "{output}");
    assert!(
        !output.contains("super-secret-login-key"),
        "the terminal echoed the secret: {output}"
    );

    let auth_path = data.join("zuno/auth.json");
    let auth: serde_json::Value =
        serde_json::from_slice(&fs::read(&auth_path).expect("read stored credential"))
            .expect("parse stored credential");
    assert_eq!(auth["acme"]["type"], "api");
    assert_eq!(auth["acme"]["key"], "super-secret-login-key");
    assert_eq!(
        fs::metadata(auth_path)
            .expect("credential metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[test]
fn bare_auth_login_selects_bedrock_and_stores_a_hidden_bearer_token() {
    let root = tempfile::tempdir().expect("temporary login environment");
    let models = root.path().join("models.json");
    fs::write(&models, "{}").expect("write empty provider catalog");

    let data = root.path().join("data");
    let config = root.path().join("config");
    let cache = root.path().join("cache");
    let home = root.path().join("home");
    for directory in [&data, &config, &cache, &home] {
        fs::create_dir_all(directory).expect("create isolated directory");
    }
    let zuno_config = config.join("zuno");
    fs::create_dir_all(&zuno_config).expect("create Zuno config directory");
    fs::write(
        zuno_config.join("zuno.json"),
        r#"{
          "provider": {
            "amazon-bedrock": {
              "name": "Amazon Bedrock",
              "transport": "bedrock",
              "models": {"claude": {"name": "Claude"}}
            }
          }
        }"#,
    )
    .expect("write configured provider");

    let mut terminal = TestPty::spawn(
        root.path(),
        &[
            ("HOME", home.as_path()),
            ("XDG_DATA_HOME", data.as_path()),
            ("XDG_CONFIG_HOME", config.as_path()),
            ("XDG_CACHE_HOME", cache.as_path()),
            ("ZUNO_MODELS_PATH", models.as_path()),
        ],
    );
    assert!(
        terminal.wait_for_output("Select provider"),
        "{}",
        terminal.output()
    );
    terminal.write(b"amazon-bedrock\r");
    assert!(
        terminal.wait_for_output("Amazon Bedrock authentication priority"),
        "{}",
        terminal.output()
    );
    assert!(
        terminal.wait_for_output("Enter Amazon Bedrock bearer token"),
        "{}",
        terminal.output()
    );
    terminal.write(b"bedrock-interactive-secret\r");

    let (status, output) =
        terminal.finish_after_output("Stored Amazon Bedrock bearer token for amazon-bedrock");
    assert!(status.success(), "{output}");
    for expected in [
        "Select provider: Amazon Bedrock",
        "AWS_BEARER_TOKEN_BEDROCK",
        "AWS credential chain",
        "Stored Amazon Bedrock bearer token for amazon-bedrock",
    ] {
        assert!(output.contains(expected), "{output}");
    }
    assert!(
        !output.contains("bedrock-interactive-secret"),
        "the terminal echoed the bearer token: {output}"
    );

    let auth_path = data.join("zuno/auth.json");
    let auth: serde_json::Value =
        serde_json::from_slice(&fs::read(&auth_path).expect("read stored credential"))
            .expect("parse stored credential");
    assert_eq!(auth["amazon-bedrock"]["type"], "api");
    assert_eq!(auth["amazon-bedrock"]["key"], "bedrock-interactive-secret");
    assert_eq!(
        fs::metadata(auth_path)
            .expect("credential metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[test]
fn bare_auth_login_hides_catalog_only_and_credential_only_providers() {
    let root = tempfile::tempdir().expect("temporary login environment");
    let models = root.path().join("models.json");
    fs::write(
        &models,
        r#"{
          "acme": {
            "id": "acme",
            "name": "Acme Catalog Only",
            "api": "https://acme.example.test/v1",
            "npm": "@ai-sdk/openai-compatible",
            "env": ["ACME_API_KEY"],
            "models": {
              "acme-test": {"id": "acme-test", "name": "Acme Test"}
            }
          }
        }"#,
    )
    .expect("write provider catalog");

    let data = root.path().join("data");
    let config = root.path().join("config");
    let cache = root.path().join("cache");
    let home = root.path().join("home");
    for directory in [&data, &config, &cache, &home] {
        fs::create_dir_all(directory).expect("create isolated directory");
    }
    let auth_dir = data.join("zuno");
    fs::create_dir_all(&auth_dir).expect("create auth directory");
    fs::write(
        auth_dir.join("auth.json"),
        r#"{"kiro-auth":{"type":"api","key":"old-key"}}"#,
    )
    .expect("write orphan credential");

    let mut terminal = TestPty::spawn(
        root.path(),
        &[
            ("HOME", home.as_path()),
            ("XDG_DATA_HOME", data.as_path()),
            ("XDG_CONFIG_HOME", config.as_path()),
            ("XDG_CACHE_HOME", cache.as_path()),
            ("ZUNO_MODELS_PATH", models.as_path()),
        ],
    );
    assert!(
        terminal.wait_for_frame("Select provider"),
        "{}",
        terminal.output()
    );
    let output = terminal.output();
    assert!(output.contains("OpenAI"), "{output}");
    assert!(output.contains("Amazon Bedrock"), "{output}");
    assert!(output.contains("OpenAI-compatible"), "{output}");
    assert!(!output.contains("Acme Catalog Only"), "{output}");
    assert!(!output.contains("kiro-auth"), "{output}");
    assert!(!output.contains("Other"), "{output}");

    terminal.write(b"\x1b");
    let (status, output) = terminal.finish_after_output("provider login cancelled");
    assert!(!status.success(), "{output}");
    assert!(output.contains("provider login cancelled"), "{output}");
}

#[test]
fn explicit_unsupported_provider_fails_before_reading_or_storing_a_key() {
    let root = tempfile::tempdir().expect("temporary login environment");
    let models = root.path().join("models.json");
    fs::write(&models, "{}").expect("write empty provider catalog");
    let data = root.path().join("data");
    let config = root.path().join("config");
    let cache = root.path().join("cache");
    let home = root.path().join("home");
    for directory in [&data, &config, &cache, &home] {
        fs::create_dir_all(directory).expect("create isolated directory");
    }

    let output = Command::new(env!("CARGO_BIN_EXE_zuno"))
        .args(["auth", "login", "kiro-auth"])
        .env_clear()
        .env("HOME", &home)
        .env("XDG_DATA_HOME", &data)
        .env("XDG_CONFIG_HOME", &config)
        .env("XDG_CACHE_HOME", &cache)
        .env("ZUNO_MODELS_PATH", &models)
        .env("ZUNO_DISABLE_PROJECT_CONFIG", "1")
        .output()
        .expect("run unsupported login");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("has no configured login capability"),
        "{stderr}"
    );
    assert!(!data.join("zuno/auth.json").exists());
}

#[test]
fn openai_login_prompts_for_the_authentication_method() {
    let root = tempfile::tempdir().expect("temporary login environment");
    let models = root.path().join("models.json");
    fs::write(&models, "{}").expect("write empty provider catalog");

    let data = root.path().join("data");
    let config = root.path().join("config");
    let cache = root.path().join("cache");
    let home = root.path().join("home");
    for directory in [&data, &config, &cache, &home] {
        fs::create_dir_all(directory).expect("create isolated directory");
    }

    let mut terminal = TestPty::spawn(
        root.path(),
        &[
            ("HOME", home.as_path()),
            ("XDG_DATA_HOME", data.as_path()),
            ("XDG_CONFIG_HOME", config.as_path()),
            ("XDG_CACHE_HOME", cache.as_path()),
            ("ZUNO_MODELS_PATH", models.as_path()),
        ],
    );
    assert!(
        terminal.wait_for_output("Select provider"),
        "{}",
        terminal.output()
    );
    terminal.write(b"\r");
    assert!(
        terminal.wait_for_frame("OpenAI connection"),
        "{}",
        terminal.output()
    );
    terminal.write(b"\r");
    assert!(
        terminal.wait_for_frame("Login method"),
        "{}",
        terminal.output()
    );
    let output = terminal.output();
    assert!(output.contains("ChatGPT Plus/Pro (browser)"), "{output}");
    assert!(
        output.contains("ChatGPT Plus/Pro (device code)"),
        "{output}"
    );
    assert!(output.contains("Manually enter API key"), "{output}");

    terminal.write(b"\x1b");
    let (status, output) = terminal.finish_after_output("provider login cancelled");
    assert!(!status.success(), "{output}");
    assert!(output.contains("provider login cancelled"), "{output}");
}

#[test]
fn empty_config_can_set_up_bedrock_with_one_visible_provider_and_the_aws_chain() {
    let fixture = EmptyLoginFixture::new();
    let mut terminal = fixture.spawn();
    assert!(
        terminal.wait_for_frame("Select provider"),
        "{}",
        terminal.output()
    );
    let picker = terminal.output();
    assert!(picker.contains("Amazon Bedrock"), "{picker}");
    assert!(!picker.contains("Bedrock Mantle"), "{picker}");
    assert!(!picker.contains("Bedrock Runtime"), "{picker}");
    assert!(!picker.contains("Converse"), "{picker}");
    terminal.write(b"bedrock\r");

    for (prompt, answer) in [
        ("AWS region", b"\r".as_slice()),
        ("AWS profile", b"us\r".as_slice()),
    ] {
        assert!(
            terminal.wait_for_output(prompt),
            "missing {prompt}: {}",
            terminal.output()
        );
        terminal.write(answer);
    }
    assert!(
        terminal.wait_for_frame("Amazon Bedrock authentication"),
        "{}",
        terminal.output()
    );
    terminal.write(b"\r");

    let (status, output) =
        terminal.finish_after_output("Amazon Bedrock will use the AWS credential chain");
    assert!(status.success(), "{output}");
    assert!(!output.contains("Bedrock model id"), "{output}");
    assert!(!output.contains("Model display name"), "{output}");
    assert!(!output.contains("as the default model?"), "{output}");
    assert!(
        output.contains("Amazon Bedrock will use the AWS credential chain"),
        "{output}"
    );
    let config = fixture.config();
    assert!(config.get("model").is_none(), "{config:#}");
    let provider = &config["provider"]["amazon-bedrock"];
    assert_eq!(provider["name"], "Amazon Bedrock");
    assert!(provider.get("models").is_none(), "{provider:#}");
    assert!(provider.get("transport").is_none(), "{provider:#}");
    assert!(provider.get("surface").is_none(), "{provider:#}");
    assert_eq!(provider["options"]["region"], "us-east-2");
    assert_eq!(provider["options"]["profile"], "us");
    assert!(!fixture.auth_path().exists());
}

#[test]
fn bedrock_setup_without_a_catalog_fails_before_asking_for_configuration() {
    let fixture = EmptyLoginFixture::new();
    fs::write(&fixture.models, "{}").expect("remove Bedrock from provider catalog");
    let config_path = fixture.config.join("zuno/zuno.json");
    fs::create_dir_all(config_path.parent().expect("config parent"))
        .expect("create config directory");
    let original_config = br#"{"formatter":false}"#;
    fs::write(&config_path, original_config).expect("seed unrelated config");
    let mut terminal = fixture.spawn();
    assert!(
        terminal.wait_for_frame("Select provider"),
        "{}",
        terminal.output()
    );
    terminal.write(b"bedrock\r");

    let (status, output) =
        terminal.finish_after_output("Amazon Bedrock is unavailable in the model catalog");
    assert!(!status.success(), "{output}");
    assert!(
        output.contains("Amazon Bedrock is unavailable in the model catalog"),
        "{output}"
    );
    assert!(output.contains("zuno models --refresh"), "{output}");
    assert!(!output.contains("Bedrock model id"), "{output}");
    assert!(!output.contains("AWS region"), "{output}");
    assert_eq!(
        fs::read(config_path).expect("read unchanged config"),
        original_config
    );
    assert!(!fixture.auth_path().exists());
}

#[test]
fn empty_config_can_set_up_openai_compatible_as_chat_completions() {
    let fixture = EmptyLoginFixture::new();
    let mut terminal = fixture.spawn();
    assert!(
        terminal.wait_for_frame("Select provider"),
        "{}",
        terminal.output()
    );
    terminal.write(b"compatible\r");
    for (prompt, answer) in [
        ("Provider id", b"local-compatible\r".as_slice()),
        ("Provider display name", b"\r".as_slice()),
        ("Base URL", b"http://127.0.0.1:8000/v1\r".as_slice()),
        ("Model id", b"local-model\r".as_slice()),
        ("Model display name", b"\r".as_slice()),
    ] {
        assert!(
            terminal.wait_for_output(prompt),
            "missing {prompt}: {}",
            terminal.output()
        );
        terminal.write(answer);
    }
    assert!(
        terminal.wait_for_frame("Use local-compatible/local-model as the default model?"),
        "{}",
        terminal.output()
    );
    terminal.write(b"\r");
    assert!(
        terminal.wait_for_output("Enter API key"),
        "{}",
        terminal.output()
    );
    terminal.write(b"compatible-secret\r");

    let (status, output) = terminal.finish_after_output("Stored API key for local-compatible");
    assert!(status.success(), "{output}");
    let config = fixture.config();
    assert_eq!(
        config["provider"]["local-compatible"]["transport"],
        "openai-compatible"
    );
    assert_eq!(config["provider"]["local-compatible"]["surface"], "chat");
    assert_eq!(
        fixture.auth()["local-compatible"]["key"],
        "compatible-secret"
    );
}

#[test]
fn openai_custom_endpoint_is_written_as_native_responses() {
    let fixture = EmptyLoginFixture::new();
    let mut terminal = fixture.spawn();
    assert!(
        terminal.wait_for_frame("Select provider"),
        "{}",
        terminal.output()
    );
    terminal.write(b"\r");
    assert!(
        terminal.wait_for_frame("OpenAI connection"),
        "{}",
        terminal.output()
    );
    terminal.write(b"custom\r");
    for (prompt, answer) in [
        ("Provider id", b"custom-responses\r".as_slice()),
        ("Provider display name", b"\r".as_slice()),
        (
            "Responses base URL",
            b"https://gateway.example.test/v1\r".as_slice(),
        ),
        ("Model id", b"reasoning-model\r".as_slice()),
        ("Model display name", b"\r".as_slice()),
    ] {
        assert!(
            terminal.wait_for_output(prompt),
            "missing {prompt}: {}",
            terminal.output()
        );
        terminal.write(answer);
    }
    assert!(
        terminal.wait_for_frame("Use custom-responses/reasoning-model as the default model?"),
        "{}",
        terminal.output()
    );
    terminal.write(b"\r");
    assert!(
        terminal.wait_for_output("Enter API key"),
        "{}",
        terminal.output()
    );
    terminal.write(b"responses-secret\r");

    let (status, output) = terminal.finish_after_output("Stored API key for custom-responses");
    assert!(status.success(), "{output}");
    assert!(
        !output.contains("responses-secret"),
        "secret input was echoed before raw mode became active: {output}"
    );
    let config = fixture.config();
    assert_eq!(
        config["provider"]["custom-responses"]["transport"],
        "openai"
    );
    assert_eq!(
        config["provider"]["custom-responses"]["surface"],
        "responses"
    );
    assert_eq!(
        fixture.auth()["custom-responses"]["key"],
        "responses-secret"
    );
}

/// A URL login shows the remote-chosen command and waits for an explicit Yes.
///
/// Enter on the untouched prompt is "No": the first row declines, so a user who
/// reflexively confirms a prompt they did not read runs nothing.
#[test]
fn url_login_shows_the_remote_command_and_enter_declines_it() {
    let root = tempfile::tempdir().expect("temporary login environment");
    let data = root.path().join("data");
    let config = root.path().join("config");
    let cache = root.path().join("cache");
    let home = root.path().join("home");
    for directory in [&data, &config, &cache, &home] {
        fs::create_dir_all(directory).expect("create isolated directory");
    }
    let marker = root.path().join("remote-command-ran");
    let fixture = WellKnownFixture::serve(serde_json::json!({
        "auth": {
            "command": ["sh", "-c", format!("echo spawned > '{}' && echo TOKEN", marker.display())],
            "env": "ACME_TOKEN"
        }
    }));

    let mut terminal = TestPty::spawn_with_args(
        root.path(),
        &[fixture.base_url.as_str()],
        &[
            ("HOME", home.as_path()),
            ("XDG_DATA_HOME", data.as_path()),
            ("XDG_CONFIG_HOME", config.as_path()),
            ("XDG_CACHE_HOME", cache.as_path()),
        ],
    );
    assert!(
        terminal.wait_for_frame("Run this command"),
        "{}",
        terminal.output()
    );
    let shown = terminal.output();
    assert!(shown.contains("program: \"sh\""), "{shown}");
    assert!(shown.contains("echo spawned"), "{shown}");
    assert!(shown.contains("the remote host did"), "{shown}");
    assert!(
        !marker.exists(),
        "the command ran before the prompt was answered"
    );

    terminal.write(b"\r");
    let (status, output) = terminal.finish_after_output("well-known provider login cancelled");
    assert!(!status.success(), "{output}");
    assert!(output.contains("Run this command: No"), "{output}");
    assert!(
        output.contains("well-known provider login cancelled"),
        "{output}"
    );
    assert!(
        !marker.exists(),
        "declining the prompt must not run the command"
    );
    assert!(
        !data.join("zuno/auth.json").exists(),
        "declining the prompt must not store a credential"
    );
    assert_eq!(
        fixture.hits(),
        1,
        "the document is fetched once, to show the command"
    );
}

/// A loopback `/.well-known/zuno` server that counts the requests it answered.
struct WellKnownFixture {
    base_url: String,
    hits: Arc<std::sync::atomic::AtomicUsize>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl WellKnownFixture {
    fn serve(document: serde_json::Value) -> Self {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback fixture");
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
        self.hits.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl Drop for WellKnownFixture {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Last line of every `terminal_prompt` frame, and so the point at which one is complete.
const FRAME_END: &str = "esc cancel";

struct TestPty {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    writer: Option<Box<dyn Write + Send>>,
    output: Arc<Mutex<Vec<u8>>>,
    reader: Option<JoinHandle<std::io::Result<()>>>,
}

struct EmptyLoginFixture {
    root: tempfile::TempDir,
    data: std::path::PathBuf,
    config: std::path::PathBuf,
    cache: std::path::PathBuf,
    home: std::path::PathBuf,
    models: std::path::PathBuf,
}

impl EmptyLoginFixture {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("temporary login environment");
        let data = root.path().join("data");
        let config = root.path().join("config");
        let cache = root.path().join("cache");
        let home = root.path().join("home");
        let models = root.path().join("models.json");
        for directory in [&data, &config, &cache, &home] {
            fs::create_dir_all(directory).expect("create isolated directory");
        }
        fs::write(
            &models,
            r#"{
              "amazon-bedrock": {
                "id": "amazon-bedrock",
                "name": "Amazon Bedrock",
                "npm": "@ai-sdk/amazon-bedrock",
                "env": ["AWS_BEARER_TOKEN_BEDROCK"],
                "models": {
                  "anthropic.claude-3-5-sonnet-20241022-v2:0": {
                    "id": "anthropic.claude-3-5-sonnet-20241022-v2:0",
                    "name": "Claude 3.5 Sonnet"
                  }
                }
              }
            }"#,
        )
        .expect("write provider catalog");
        Self {
            root,
            data,
            config,
            cache,
            home,
            models,
        }
    }

    fn spawn(&self) -> TestPty {
        TestPty::spawn(
            self.root.path(),
            &[
                ("HOME", self.home.as_path()),
                ("XDG_DATA_HOME", self.data.as_path()),
                ("XDG_CONFIG_HOME", self.config.as_path()),
                ("XDG_CACHE_HOME", self.cache.as_path()),
                ("ZUNO_MODELS_PATH", self.models.as_path()),
            ],
        )
    }

    fn config(&self) -> serde_json::Value {
        serde_json::from_slice(
            &fs::read(self.config.join("zuno/zuno.json")).expect("read configured provider"),
        )
        .expect("parse configured provider")
    }

    fn auth_path(&self) -> std::path::PathBuf {
        self.data.join("zuno/auth.json")
    }

    fn auth(&self) -> serde_json::Value {
        serde_json::from_slice(&fs::read(self.auth_path()).expect("read stored credential"))
            .expect("parse stored credential")
    }
}

impl TestPty {
    fn spawn(cwd: &std::path::Path, variables: &[(&str, &std::path::Path)]) -> Self {
        Self::spawn_with_args(cwd, &[], variables)
    }

    /// Spawn `zuno auth login <args...>` in a fresh PTY.
    fn spawn_with_args(
        cwd: &std::path::Path,
        args: &[&str],
        variables: &[(&str, &std::path::Path)],
    ) -> Self {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 100,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("open authentication PTY");
        let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_zuno"));
        command.args(["auth", "login"]);
        command.args(args);
        command.env_clear();
        command.env("TERM", "xterm-256color");
        command.env("ZUNO_DISABLE_PROJECT_CONFIG", "1");
        for (key, value) in variables {
            command.env(key, value);
        }
        command.cwd(cwd);
        let child = pair
            .slave
            .spawn_command(command)
            .expect("spawn authentication command");
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader().expect("clone PTY reader");
        let writer = pair.master.take_writer().expect("take PTY writer");
        let output = Arc::new(Mutex::new(Vec::new()));
        let reader_output = Arc::clone(&output);
        let reader = std::thread::spawn(move || {
            let mut buffer = [0_u8; 1024];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => return Ok(()),
                    Ok(read) => reader_output
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .extend_from_slice(&buffer[..read]),
                    Err(error) if error.raw_os_error() == Some(5) => return Ok(()),
                    Err(error) => return Err(error),
                }
            }
        });

        Self {
            child,
            writer: Some(writer),
            output,
            reader: Some(reader),
        }
    }

    fn write(&mut self, input: &[u8]) {
        let writer = self.writer.as_mut().expect("PTY writer");
        writer.write_all(input).expect("write PTY input");
        writer.flush().expect("flush PTY input");
    }

    fn wait_for_output(&mut self, expected: &str) -> bool {
        self.wait_until(|output| output.contains(expected))
    }

    /// Wait for a whole prompt frame, not just the line that names it.
    ///
    /// `TerminalSession::render` writes each line of a frame with its own unbuffered write
    /// and flushes once at the end, so a reader can hold `? Select provider` and the search
    /// line while the choice rows are still in flight — asserting on a row then reads a
    /// frame that has none yet. Every frame ends with the key hints, so their arrival after
    /// the message is what makes the rows between them readable. Looking only at the text
    /// that follows the last occurrence of the message keeps the hints of an earlier frame
    /// from settling a later prompt.
    fn wait_for_frame(&mut self, message: &str) -> bool {
        self.wait_until(|output| {
            output.contains(message)
                && output
                    .rsplit(message)
                    .next()
                    .is_some_and(|tail| tail.contains(FRAME_END))
        })
    }

    fn wait_until(&mut self, settled: impl Fn(&str) -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if settled(&self.output()) {
                return true;
            }
            if self
                .child
                .try_wait()
                .expect("poll authentication command")
                .is_some()
            {
                return settled(&self.output());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    /// Wait for one semantic terminal outcome, then require prompt process teardown.
    ///
    /// Under the full Linux suite the child can be descheduled for several seconds
    /// after the final secret is submitted. A single short exit deadline confuses
    /// that pre-outcome scheduling delay with a process that printed completion but
    /// leaked. Keep those failures separate: the command has a bounded window to
    /// produce the exact outcome the test owns, and only then receives a short exit
    /// grace period.
    fn finish_after_output(&mut self, expected: &str) -> (portable_pty::ExitStatus, String) {
        let outcome_deadline = Instant::now() + Duration::from_secs(30);
        let mut outcome_observed_at = None;
        loop {
            if let Some(status) = self.child.try_wait().expect("poll authentication command") {
                self.writer.take();
                self.join_reader();
                let output = self.output();
                assert!(
                    output.contains(expected),
                    "authentication command exited before terminal outcome {expected:?}: {output}"
                );
                return (status, output);
            }

            let now = Instant::now();
            let output = self.output();
            if outcome_observed_at.is_none() && output.contains(expected) {
                outcome_observed_at = Some(now);
            }
            let timed_out = outcome_observed_at.map_or_else(
                || now >= outcome_deadline,
                |observed| now.duration_since(observed) >= Duration::from_secs(5),
            );
            if timed_out {
                let output = self.output();
                let _ = self.child.kill();
                self.writer.take();
                self.join_reader();
                if outcome_observed_at.is_some() {
                    panic!(
                        "authentication command produced terminal outcome {expected:?} \
                         but did not exit: {output}"
                    );
                }
                panic!(
                    "authentication command did not produce terminal outcome {expected:?}: \
                     {output}"
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn output(&self) -> String {
        String::from_utf8_lossy(&self.output.lock().unwrap_or_else(PoisonError::into_inner))
            .into_owned()
    }

    fn join_reader(&mut self) {
        if let Some(reader) = self.reader.take() {
            reader
                .join()
                .expect("join PTY reader")
                .expect("read PTY output");
        }
    }
}

impl Drop for TestPty {
    fn drop(&mut self) {
        let _ = self.child.kill();
        self.writer.take();
        self.join_reader();
    }
}
