use std::path::Path;

use anyhow::Result;
use serde_json::Value;
use serde_json::json;
use tempfile::TempDir;

fn zuno_command(codex_home: &Path) -> Result<assert_cmd::Command> {
    let mut command = assert_cmd::Command::new(codex_utils_cargo_bin::cargo_bin("zuno")?);
    command.env("CODEX_HOME", codex_home);
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
