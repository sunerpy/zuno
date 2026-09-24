use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::path::Path;
use std::process::Child;
use std::process::Command;
use std::process::Stdio;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::create_escalated_command_execution_sse_response;
use app_test_support::create_final_assistant_message_sse_response;
use app_test_support::create_mock_responses_server_sequence;
use serde_json::Value;
use serde_json::json;
use tempfile::TempDir;

fn zuno_command(codex_home: &Path) -> Result<assert_cmd::Command> {
    let mut command = assert_cmd::Command::new(codex_utils_cargo_bin::cargo_bin("zuno")?);
    command.env("ZUNO_HOME", codex_home);
    Ok(command)
}

#[test]
fn native_acp_initializes_over_stdio_and_exits_on_eof() -> Result<()> {
    let codex_home = TempDir::new()?;
    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": 1,
            "clientInfo": {"name": "zuno-cli-test", "version": "0"}
        }
    });

    let output = zuno_command(codex_home.path())?
        .arg("acp")
        .write_stdin(format!("{request}\n"))
        .output()?;

    anyhow::ensure!(
        output.status.success(),
        "ACP process failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout)?;
    let response: Value = serde_json::from_str(stdout.trim())?;
    assert_eq!(response["jsonrpc"], "2.0");
    assert_eq!(response["id"], 1);
    assert_eq!(response["result"]["protocolVersion"], 1);
    assert_eq!(response["result"]["agentInfo"]["name"], "Zuno");

    Ok(())
}

/// Kills the ACP process when a test returns early, so a hung bridge cannot
/// keep the test binary alive.
struct AcpProcess {
    child: Child,
    stdin: std::process::ChildStdin,
    frames: mpsc::Receiver<String>,
}

impl AcpProcess {
    fn spawn(codex_home: &Path) -> Result<Self> {
        let mut child = Command::new(codex_utils_cargo_bin::cargo_bin("zuno")?)
            .arg("acp")
            .env("ZUNO_HOME", codex_home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .context("spawn zuno acp")?;
        let stdin = child.stdin.take().context("acp stdin")?;
        let stdout = child.stdout.take().context("acp stdout")?;
        let (tx, frames) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if line.trim().is_empty() {
                    continue;
                }
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Ok(Self {
            child,
            stdin,
            frames,
        })
    }

    fn send(&mut self, frame: Value) -> Result<()> {
        writeln!(self.stdin, "{frame}")?;
        self.stdin.flush()?;
        Ok(())
    }

    /// Reads frames until `accept` returns `Some`, failing instead of hanging when
    /// the bridge stops answering.
    fn read_until<T>(&self, what: &str, mut accept: impl FnMut(&Value) -> Option<T>) -> Result<T> {
        loop {
            let line = self
                .frames
                .recv_timeout(Duration::from_secs(90))
                .with_context(|| format!("timed out waiting for {what} from the ACP bridge"))?;
            let frame: Value = serde_json::from_str(&line)
                .with_context(|| format!("ACP bridge wrote a non-JSON frame: {line}"))?;
            if let Some(value) = accept(&frame) {
                return Ok(value);
            }
        }
    }
}

impl Drop for AcpProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A command approval is a round trip: the App Server asks the bridge, the bridge
/// asks the ACP client with `session/request_permission`, and the client's answer
/// must reach the bridge while the turn is still waiting on it. The bridge used
/// to await that answer inside the same loop that reads client frames, so the
/// answer was never read and the turn hung forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_acp_completes_a_turn_after_the_client_answers_a_permission_request() -> Result<()> {
    let codex_home = TempDir::new()?;
    let workspace = TempDir::new()?;
    let responses = vec![
        create_escalated_command_execution_sse_response(
            vec!["echo".to_string(), "acp-approval-roundtrip".to_string()],
            /*workdir*/ None,
            Some(5000),
            "call-approve",
        )?,
        create_final_assistant_message_sse_response("done")?,
    ];
    let server = create_mock_responses_server_sequence(responses).await;
    MockResponsesConfig::new(&server.uri())
        .with_approval_policy("on-request")
        .write(codex_home.path())?;

    let mut acp = AcpProcess::spawn(codex_home.path())?;
    acp.send(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": 1,
            "clientInfo": {"name": "zuno-cli-test", "version": "0"},
            "clientCapabilities": {"fs": {"readTextFile": false, "writeTextFile": false}}
        }
    }))?;
    acp.read_until("the initialize response", |frame| {
        (frame["id"] == 1).then(|| frame["result"].clone())
    })?;

    acp.send(json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "session/new",
        "params": {"cwd": workspace.path(), "mcpServers": []}
    }))?;
    let session_id = acp.read_until("the session/new response", |frame| {
        (frame["id"] == 2).then(|| frame["result"]["sessionId"].as_str().map(str::to_owned))
    })?;
    let session_id = session_id.context("session/new response omitted sessionId")?;

    acp.send(json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "session/prompt",
        "params": {
            "sessionId": session_id,
            "prompt": [{"type": "text", "text": "run the command"}]
        }
    }))?;
    let (permission_id, tool_call) = acp.read_until("session/request_permission", |frame| {
        (frame["method"] == "session/request_permission")
            .then(|| (frame["id"].clone(), frame["params"]["toolCall"].clone()))
    })?;
    assert_eq!(tool_call["kind"], "execute");
    assert_eq!(tool_call["status"], "pending");

    acp.send(json!({
        "jsonrpc": "2.0",
        "id": permission_id,
        "result": {"outcome": {"outcome": "selected", "optionId": "allow_once"}}
    }))?;
    let stop_reason = acp.read_until("the session/prompt response", |frame| {
        (frame["id"] == 3).then(|| frame.clone())
    })?;
    assert_eq!(
        stop_reason["result"]["stopReason"], "end_turn",
        "prompt response: {stop_reason}"
    );
    Ok(())
}
