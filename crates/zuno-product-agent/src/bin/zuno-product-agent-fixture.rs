use serde_json::{Value, json};
use std::io::{BufRead, Write};
use std::process::{Command, ExitCode};
use std::time::Duration;

fn main() -> ExitCode {
    if let Some(code) = zuno_process::run_guard_from_args() {
        return code;
    }
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("fixture failed: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    capture(&arguments)?;
    if arguments.first().map(String::as_str) == Some("app-server") {
        run_codex()
    } else {
        run_claude()
    }
}

fn capture(arguments: &[String]) -> Result<(), String> {
    let Some(path) = std::env::var_os("ZUNO_PRODUCT_AGENT_CAPTURE") else {
        return Ok(());
    };
    let document = json!({
        "args":arguments,
        "cwd":std::env::current_dir().map_err(|error| error.to_string())?,
        "httpProxy":std::env::var("HTTP_PROXY").ok(),
        "noProxy":std::env::var("NO_PROXY").ok(),
        "secret":std::env::var("SECRET_TOKEN").ok(),
        "threadStartRequests":[],
        "turnStartRequests":[],
    });
    std::fs::write(
        path,
        serde_json::to_vec(&document).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())
}

fn capture_request(field: &str, request: &Value) -> Result<(), String> {
    let Some(path) = std::env::var_os("ZUNO_PRODUCT_AGENT_CAPTURE") else {
        return Ok(());
    };
    let mut document: Value =
        serde_json::from_slice(&std::fs::read(&path).map_err(|error| error.to_string())?)
            .map_err(|error| error.to_string())?;
    document[field]
        .as_array_mut()
        .ok_or_else(|| format!("capture field `{field}` is not an array"))?
        .push(request.clone());
    std::fs::write(
        path,
        serde_json::to_vec(&document).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())
}

fn run_codex() -> Result<(), String> {
    let mode =
        std::env::var("ZUNO_PRODUCT_AGENT_FIXTURE_MODE").unwrap_or_else(|_| "normal".to_owned());
    let stdin = std::io::stdin();
    let mut reader = stdin.lock();
    let stdout = std::io::stdout();
    let mut writer = stdout.lock();
    let mut line = String::new();
    loop {
        line.clear();
        if reader
            .read_line(&mut line)
            .map_err(|error| error.to_string())?
            == 0
        {
            return Ok(());
        }
        let request: Value =
            serde_json::from_str(line.trim_end()).map_err(|error| error.to_string())?;
        let method = request
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match method {
            "initialize" if mode == "incompatible" => {
                send(
                    &mut writer,
                    json!({"id":request["id"],"error":{"code":-32602,"message":"unsupported protocol version"}}),
                )?;
                return Ok(());
            }
            "initialize" => {
                send(
                    &mut writer,
                    json!({"id":request["id"],"result":{"serverInfo":{"name":"fixture"}}}),
                )?;
            }
            "initialized" => {}
            "thread/start" => {
                capture_request("threadStartRequests", &request)?;
                if mode == "legacy"
                    && request.pointer("/params/sandbox").and_then(Value::as_str)
                        == Some("workspaceWrite")
                {
                    send(
                        &mut writer,
                        json!({"id":request["id"],"error":{"code":-32602,"message":"legacy enum spelling required"}}),
                    )?;
                    continue;
                }
                if mode == "approval" {
                    send(
                        &mut writer,
                        json!({
                            "id":90,
                            "method":"item/commandExecution/requestApproval",
                            "params":{"command":"echo fixture"}
                        }),
                    )?;
                    line.clear();
                    reader
                        .read_line(&mut line)
                        .map_err(|error| error.to_string())?;
                    let response: Value =
                        serde_json::from_str(line.trim_end()).map_err(|error| error.to_string())?;
                    if response.pointer("/result/decision").and_then(Value::as_str)
                        != Some("decline")
                    {
                        return Err(format!("expected unattended decline, got {response}"));
                    }
                }
                send(
                    &mut writer,
                    json!({"id":request["id"],"result":{"thread":{"id":"thr_fixture"}}}),
                )?;
            }
            "turn/start" => {
                capture_request("turnStartRequests", &request)?;
                send(
                    &mut writer,
                    json!({"id":request["id"],"result":{"turn":{"id":"turn_fixture"}}}),
                )?;
                match mode.as_str() {
                    "malformed" => {
                        eprintln!(
                            "authorization: bearer {}",
                            std::env::var("SECRET_TOKEN").unwrap_or_default()
                        );
                        writeln!(writer, "{{not-json").map_err(|error| error.to_string())?;
                        writer.flush().map_err(|error| error.to_string())?;
                        return Ok(());
                    }
                    "permission-denied" => {
                        send(
                            &mut writer,
                            json!({"method":"turn/completed","params":{"turn":{"status":"failed","items":[],"error":{"message":"permission denied"}}}}),
                        )?;
                        return Ok(());
                    }
                    "hang" => return hang_with_child(),
                    "eof" => return Ok(()),
                    _ => {
                        let text = if mode == "approval" {
                            "approval declined safely"
                        } else {
                            "codex final answer"
                        };
                        send(
                            &mut writer,
                            json!({"method":"item/completed","params":{"item":{"type":"agentMessage","text":text}}}),
                        )?;
                        send(
                            &mut writer,
                            json!({"method":"turn/completed","params":{"turn":{"status":"completed","items":[{"type":"agentMessage","text":text}],"error":null}}}),
                        )?;
                        return Ok(());
                    }
                }
            }
            "turn/interrupt" => return Ok(()),
            _ => {}
        }
    }
}

fn run_claude() -> Result<(), String> {
    let mode =
        std::env::var("ZUNO_PRODUCT_AGENT_FIXTURE_MODE").unwrap_or_else(|_| "normal".to_owned());
    match mode.as_str() {
        "malformed" => {
            eprintln!(
                "api_key={}",
                std::env::var("SECRET_TOKEN").unwrap_or_default()
            );
            println!("not-json");
        }
        "permission-denied" => {
            println!(
                "{}",
                json!({"type":"result","subtype":"error_during_execution","is_error":true,"result":"permission denied by native policy"})
            );
        }
        "hang" => return hang_with_child(),
        _ => {
            println!(
                "{}",
                json!({"type":"assistant","message":"internal stream"})
            );
            println!(
                "{}",
                json!({"type":"result","subtype":"success","is_error":false,"result":"claude final answer"})
            );
        }
    }
    Ok(())
}

fn send(writer: &mut impl Write, value: Value) -> Result<(), String> {
    serde_json::to_writer(&mut *writer, &value).map_err(|error| error.to_string())?;
    writer.write_all(b"\n").map_err(|error| error.to_string())?;
    writer.flush().map_err(|error| error.to_string())
}

fn hang_with_child() -> Result<(), String> {
    let mut child = Command::new("sh")
        .args(["-c", "sleep 300"])
        .spawn()
        .map_err(|error| error.to_string())?;
    if let Some(path) = std::env::var_os("ZUNO_PRODUCT_AGENT_CHILD_PID") {
        std::fs::write(path, child.id().to_string()).map_err(|error| error.to_string())?;
    }
    loop {
        if child
            .try_wait()
            .map_err(|error| error.to_string())?
            .is_some()
        {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
