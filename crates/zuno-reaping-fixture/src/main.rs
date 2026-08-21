use std::error::Error;
use std::ffi::OsString;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use zuno_catalog::lsp_config::ResolvedLsp;
use zuno_config::schema::lsp::LspConfig;
use zuno_config::schema::mcp::{LocalKind, McpLocal};
use zuno_lsp::{Manager, RestartPolicy, ServerRegistry};
use zuno_mcp::StdioClient;
use zuno_pty::{CreateInput, PtyId, PtyService};

const SESSION_COUNT: usize = 2;

fn main() -> ExitCode {
    if let Some(code) = zuno_process::run_guard_from_args() {
        return code;
    }
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("reaping fixture failed: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let mut arguments = std::env::args_os().skip(1);
    let mode = next_utf8(&mut arguments, "mode")?;
    match mode.as_str() {
        "parent" => {
            let ready = next_path(&mut arguments, "ready path")?;
            let stop = next_path(&mut arguments, "stop path")?;
            let root = next_path(&mut arguments, "workspace path")?;
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(4)
                .enable_all()
                .build()?;
            runtime.block_on(run_parent(&ready, &stop, &root))
        }
        "lsp" => run_lsp(),
        "mcp" => run_mcp(),
        "pty" => run_sleeping_child(),
        "grandchild" => loop {
            std::thread::sleep(Duration::from_secs(60));
        },
        _ => Err(format!("unknown fixture mode {mode}").into()),
    }
}

async fn run_parent(ready: &Path, stop: &Path, root: &Path) -> Result<(), Box<dyn Error>> {
    let executable = std::env::current_exe()?;
    zuno_process::activate_guard_executable(&executable)?;
    std::fs::create_dir_all(root)?;

    let mut lsp = Vec::with_capacity(SESSION_COUNT);
    for index in 0..SESSION_COUNT {
        let file = root.join(format!("session-{index}.g6"));
        std::fs::write(&file, "fixture")?;
        let id = format!("g6-lsp-{index}");
        let config: LspConfig = serde_json::from_value(json!({
            id: {
                "command": [executable.to_string_lossy(), "lsp"],
                "extensions": [".g6"]
            }
        }))?;
        let registry = Arc::new(ServerRegistry::offline(&ResolvedLsp::resolve(Some(
            &config,
        ))));
        let manager = Manager::new(root, registry, RestartPolicy::default());
        manager.touch_file(&file).await?;
        lsp.push(manager);
    }

    let mut mcp = Vec::with_capacity(SESSION_COUNT);
    for index in 0..SESSION_COUNT {
        let config = McpLocal {
            kind: LocalKind::Local,
            command: vec![executable.to_string_lossy().into_owned(), "mcp".to_owned()],
            cwd: None,
            environment: None,
            enabled: None,
            timeout: None,
        };
        mcp.push(StdioClient::connect(format!("g6-mcp-{index}"), root, &config).await?);
    }

    let pty = PtyService::new(root);
    let mut pty_ids = Vec::with_capacity(SESSION_COUNT);
    for _ in 0..SESSION_COUNT {
        let info = pty.create(CreateInput {
            command: Some(executable.to_string_lossy().into_owned()),
            args: Some(vec!["pty".to_owned()]),
            ..CreateInput::default()
        })?;
        pty_ids.push(info.id);
    }

    std::fs::write(ready, b"ready")?;
    while !stop.exists() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    shutdown_hosts(lsp, mcp, pty, pty_ids).await;
    Ok(())
}

async fn shutdown_hosts(
    lsp: Vec<Manager>,
    mcp: Vec<StdioClient>,
    pty: PtyService,
    pty_ids: Vec<PtyId>,
) {
    for client in mcp {
        client.close().await;
    }
    for manager in lsp {
        manager.shutdown().await;
    }
    for id in pty_ids {
        let _removed = pty.remove(&id);
    }
}

fn run_lsp() -> Result<(), Box<dyn Error>> {
    let _grandchild = spawn_grandchild()?;
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let stdout = io::stdout();
    let mut output = stdout.lock();
    while let Some(message) = read_lsp_frame(&mut input)? {
        match message.get("method").and_then(Value::as_str) {
            Some("initialize") => write_lsp_frame(
                &mut output,
                &json!({"jsonrpc":"2.0","id":message["id"],"result":{"capabilities":{}}}),
            )?,
            Some("shutdown") => write_lsp_frame(
                &mut output,
                &json!({"jsonrpc":"2.0","id":message["id"],"result":null}),
            )?,
            Some("exit") => break,
            _ => {}
        }
    }
    Ok(())
}

fn run_mcp() -> Result<(), Box<dyn Error>> {
    let _grandchild = spawn_grandchild()?;
    run_line_protocol(|message| {
        (message.get("method").and_then(Value::as_str) == Some("initialize")).then(|| {
            json!({
                "jsonrpc":"2.0",
                "id":message["id"],
                "result":{
                    "protocolVersion":zuno_mcp::PROTOCOL_VERSION,
                    "capabilities":{},
                    "serverInfo":{"name":"g6-mcp","version":"1"}
                }
            })
        })
    })
}

fn run_sleeping_child() -> Result<(), Box<dyn Error>> {
    let _grandchild = spawn_grandchild()?;
    loop {
        std::thread::sleep(Duration::from_secs(60));
    }
}

fn run_line_protocol(
    mut response: impl FnMut(&Value) -> Option<Value>,
) -> Result<(), Box<dyn Error>> {
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let stdout = io::stdout();
    let mut output = stdout.lock();
    let mut line = String::new();
    while input.read_line(&mut line)? != 0 {
        let message: Value = serde_json::from_str(&line)?;
        if let Some(message) = response(&message) {
            serde_json::to_writer(&mut output, &message)?;
            output.write_all(b"\n")?;
            output.flush()?;
        }
        line.clear();
    }
    Ok(())
}

fn read_lsp_frame(input: &mut impl BufRead) -> Result<Option<Value>, Box<dyn Error>> {
    let mut content_length = None;
    loop {
        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        if let Some(value) = line.strip_prefix("Content-Length:") {
            content_length = Some(value.trim().parse::<usize>()?);
        }
    }
    let length = content_length.ok_or("missing Content-Length")?;
    let mut body = vec![0_u8; length];
    input.read_exact(&mut body)?;
    Ok(Some(serde_json::from_slice(&body)?))
}

fn write_lsp_frame(output: &mut impl Write, message: &Value) -> Result<(), Box<dyn Error>> {
    let body = serde_json::to_vec(message)?;
    write!(output, "Content-Length: {}\r\n\r\n", body.len())?;
    output.write_all(&body)?;
    output.flush()?;
    Ok(())
}

fn spawn_grandchild() -> io::Result<Child> {
    Command::new(std::env::current_exe()?)
        .arg("grandchild")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
}

fn next_utf8(
    arguments: &mut impl Iterator<Item = OsString>,
    name: &str,
) -> Result<String, Box<dyn Error>> {
    arguments
        .next()
        .ok_or_else(|| format!("missing {name}").into())
        .and_then(|value| {
            value
                .into_string()
                .map_err(|_| format!("invalid {name}").into())
        })
}

fn next_path(
    arguments: &mut impl Iterator<Item = OsString>,
    name: &str,
) -> Result<PathBuf, Box<dyn Error>> {
    arguments
        .next()
        .map(PathBuf::from)
        .ok_or_else(|| format!("missing {name}").into())
}
