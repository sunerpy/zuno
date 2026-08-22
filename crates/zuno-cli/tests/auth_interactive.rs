#![cfg(unix)]

use std::fs;
use std::io::{Read as _, Write};
use std::os::unix::fs::PermissionsExt as _;
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
            "env": ["ACME_API_KEY"],
            "models": {}
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

    let (status, output) = terminal.finish();
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
        terminal.wait_for_output("Login method"),
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
    let (status, output) = terminal.finish();
    assert!(!status.success(), "{output}");
    assert!(output.contains("provider login cancelled"), "{output}");
}

struct TestPty {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    writer: Option<Box<dyn Write + Send>>,
    output: Arc<Mutex<Vec<u8>>>,
    reader: Option<JoinHandle<std::io::Result<()>>>,
}

impl TestPty {
    fn spawn(cwd: &std::path::Path, variables: &[(&str, &std::path::Path)]) -> Self {
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
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if self.output().contains(expected) {
                return true;
            }
            if self
                .child
                .try_wait()
                .expect("poll authentication command")
                .is_some()
            {
                return self.output().contains(expected);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    fn finish(&mut self) -> (portable_pty::ExitStatus, String) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = self.child.try_wait().expect("poll authentication command") {
                self.writer.take();
                self.join_reader();
                return (status, self.output());
            }
            if Instant::now() >= deadline {
                let output = self.output();
                let _ = self.child.kill();
                self.writer.take();
                self.join_reader();
                panic!("authentication command did not exit: {output}");
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
