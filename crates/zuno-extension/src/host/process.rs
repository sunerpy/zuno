use std::ffi::OsString;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Map, Value, json};
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader, BufWriter,
};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;

use super::{
    PLUGIN_PROTOCOL_VERSION, PluginHost, PluginHostError, PluginInvocation, PluginResult,
    RuntimeSpec, SecretRedactor,
};
use crate::PluginRuntime;

const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;
const STDERR_LIMIT: usize = 64 * 1024;
const STOP_WAIT: Duration = Duration::from_secs(2);

pub(super) async fn start(spec: RuntimeSpec) -> Result<Arc<dyn PluginHost>, PluginHostError> {
    let PluginRuntime::Process {
        command,
        args,
        timeout_ms: _,
        capabilities: _,
    } = &spec.runtime
    else {
        unreachable!("process provider received a non-process runtime");
    };
    let executable = resolve_command(&spec.root, command);
    let arguments = args.iter().map(OsString::from).collect::<Vec<_>>();
    let (program, guarded_arguments) = zuno_process::guarded_argv(&executable, &arguments);
    let mut command = Command::new(program);
    command
        .args(guarded_arguments)
        .current_dir(&spec.root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command.spawn().map_err(|error| PluginHostError::Start {
        package: spec.package.clone(),
        message: error.to_string(),
    })?;
    let stdin = child.stdin.take().ok_or_else(|| PluginHostError::Start {
        package: spec.package.clone(),
        message: "contained process did not expose stdin".to_owned(),
    })?;
    let stdout = child.stdout.take().ok_or_else(|| PluginHostError::Start {
        package: spec.package.clone(),
        message: "contained process did not expose stdout".to_owned(),
    })?;
    let stderr = child.stderr.take().ok_or_else(|| PluginHostError::Start {
        package: spec.package.clone(),
        message: "contained process did not expose stderr".to_owned(),
    })?;
    let timeout = spec.timeout();
    let capabilities = spec.capability_names();
    let host = Arc::new(ProcessPluginHost {
        package: spec.package.clone(),
        root: spec.root,
        workspace: spec.workspace,
        timeout,
        capabilities,
        redactor: SecretRedactor::from_process(),
        state: Mutex::new(Some(ProcessState {
            child,
            writer: BufWriter::new(stdin),
            reader: BufReader::new(stdout),
            stderr: tokio::spawn(read_bounded(stderr, STDERR_LIMIT)),
            next_id: 1,
        })),
    });
    if let Err(error) = host.initialize().await {
        let cleanup = host.shutdown().await;
        return match cleanup {
            Ok(()) => Err(error),
            Err(cleanup) => Err(PluginHostError::Start {
                package: spec.package,
                message: format!("{error}; startup cleanup failed: {cleanup}"),
            }),
        };
    }
    Ok(host)
}

fn resolve_command(root: &Path, command: &str) -> OsString {
    let path = Path::new(command);
    if path.is_absolute() {
        return path.as_os_str().to_owned();
    }
    let local = root.join(path);
    if local.is_file() || path.components().count() > 1 {
        local.into_os_string()
    } else {
        OsString::from(command)
    }
}

struct ProcessPluginHost {
    package: String,
    root: std::path::PathBuf,
    workspace: std::path::PathBuf,
    timeout: Duration,
    capabilities: Vec<String>,
    redactor: SecretRedactor,
    state: Mutex<Option<ProcessState>>,
}

struct ProcessState {
    child: Child,
    writer: BufWriter<ChildStdin>,
    reader: BufReader<ChildStdout>,
    stderr: tokio::task::JoinHandle<String>,
    next_id: u64,
}

impl ProcessPluginHost {
    async fn initialize(&self) -> Result<(), PluginHostError> {
        let result = self
            .rpc(
                "initialize",
                json!({
                    "protocolVersion": PLUGIN_PROTOCOL_VERSION,
                    "packageId": self.package,
                    "packageRoot": self.root,
                    "workspace": self.workspace,
                    "capabilities": self.capabilities,
                }),
                None,
                false,
            )
            .await?;
        let negotiated = result.get("protocolVersion").and_then(Value::as_str);
        if negotiated != Some(PLUGIN_PROTOCOL_VERSION) {
            return Err(PluginHostError::Incompatible {
                package: self.package.clone(),
                message: format!(
                    "initialize returned protocol `{}`; expected `{PLUGIN_PROTOCOL_VERSION}`",
                    negotiated.unwrap_or("<missing>")
                ),
            });
        }
        Ok(())
    }

    async fn rpc(
        &self,
        method: &str,
        params: Value,
        interrupt: Option<&Arc<dyn zuno_tool::InterruptHandle>>,
        uncertain_after_send: bool,
    ) -> Result<Value, PluginHostError> {
        let mut slot = self.state.lock().await;
        let Some(state) = slot.as_mut() else {
            return Err(PluginHostError::Uncertain {
                package: self.package.clone(),
                operation: method.to_owned(),
                message: "the process host is no longer running".to_owned(),
            });
        };
        if let Some(status) =
            state
                .child
                .try_wait()
                .map_err(|error| PluginHostError::Uncertain {
                    package: self.package.clone(),
                    operation: method.to_owned(),
                    message: self.redactor.safe(error.to_string()),
                })?
        {
            let mut dead = slot.take().expect("state existed above");
            drop(slot);
            let stderr = finish_stderr(&mut dead).await;
            return Err(PluginHostError::Uncertain {
                package: self.package.clone(),
                operation: method.to_owned(),
                message: diagnostic(
                    &self.redactor,
                    format!("process exited with {status}"),
                    &stderr,
                ),
            });
        }
        let id = state.next_id;
        state.next_id = state.next_id.saturating_add(1);
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let operation = exchange(state, id, request);
        let outcome = if let Some(interrupt) = interrupt {
            tokio::select! {
                result = tokio::time::timeout(self.timeout, operation) => {
                    result.map_err(|_| RpcFailure::TimedOut)
                }
                () = interrupt.notified() => Err(RpcFailure::Cancelled),
            }
        } else {
            tokio::time::timeout(self.timeout, operation)
                .await
                .map_err(|_| RpcFailure::TimedOut)
        };
        match outcome {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(RpcFailure::Peer(message))) => Err(PluginHostError::Failed {
                package: self.package.clone(),
                tool: method.to_owned(),
                message: self.redactor.safe(message),
            }),
            Ok(Err(failure)) | Err(failure) => {
                let mut failed = slot.take().expect("state existed during exchange");
                drop(slot);
                let cleanup = terminate(&mut failed).await;
                let stderr = finish_stderr(&mut failed).await;
                match failure {
                    RpcFailure::Cancelled => {
                        if let Err(cleanup) = cleanup {
                            return Err(PluginHostError::Uncertain {
                                package: self.package.clone(),
                                operation: method.to_owned(),
                                message: diagnostic(
                                    &self.redactor,
                                    format!("cancellation cleanup failed: {cleanup}"),
                                    &stderr,
                                ),
                            });
                        }
                        Err(PluginHostError::Cancelled {
                            package: self.package.clone(),
                            tool: method.to_owned(),
                        })
                    }
                    RpcFailure::TimedOut if !uncertain_after_send && cleanup.is_ok() => {
                        Err(PluginHostError::Timeout {
                            package: self.package.clone(),
                            operation: method.to_owned(),
                            elapsed: self.timeout,
                        })
                    }
                    other => Err(PluginHostError::Uncertain {
                        package: self.package.clone(),
                        operation: method.to_owned(),
                        message: diagnostic(
                            &self.redactor,
                            format!(
                                "{}{}",
                                other.message(),
                                cleanup
                                    .err()
                                    .map(|error| format!("; cleanup failed: {error}"))
                                    .unwrap_or_default()
                            ),
                            &stderr,
                        ),
                    }),
                }
            }
        }
    }

    async fn retire_uncertain(&self, operation: String, message: String) -> PluginHostError {
        let state = {
            let mut slot = self.state.lock().await;
            slot.take()
        };
        let Some(mut state) = state else {
            return PluginHostError::Uncertain {
                package: self.package.clone(),
                operation,
                message: self.redactor.safe(message),
            };
        };
        let cleanup = terminate(&mut state).await;
        let stderr = finish_stderr(&mut state).await;
        let message = match cleanup {
            Ok(()) => message,
            Err(cleanup) => format!("{message}; cleanup failed: {cleanup}"),
        };
        PluginHostError::Uncertain {
            package: self.package.clone(),
            operation,
            message: diagnostic(&self.redactor, message, &stderr),
        }
    }
}

#[async_trait]
impl PluginHost for ProcessPluginHost {
    async fn invoke(&self, request: PluginInvocation) -> Result<PluginResult, PluginHostError> {
        let tool = request.tool.clone();
        let result = self
            .rpc(
                "tools/call",
                json!({
                    "tool": request.tool,
                    "arguments": request.arguments,
                    "sessionId": request.session_id,
                    "messageId": request.message_id,
                    "callId": request.call_id,
                    "agent": request.agent,
                }),
                Some(&request.interrupt),
                true,
            )
            .await?;
        match decode_result(result) {
            Ok(result) => Ok(result),
            Err(message) => Err(self
                .retire_uncertain(format!("tool `{tool}` response"), message)
                .await),
        }
    }

    async fn shutdown(&self) -> Result<(), PluginHostError> {
        let mut state = {
            let mut slot = self.state.lock().await;
            let Some(mut state) = slot.take() else {
                return Ok(());
            };
            let id = state.next_id;
            state.next_id = state.next_id.saturating_add(1);
            let request = json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "shutdown",
                "params": {},
            });
            let _ignored =
                tokio::time::timeout(self.timeout, exchange(&mut state, id, request)).await;
            state
        };
        let cleanup = terminate(&mut state).await;
        let stderr = finish_stderr(&mut state).await;
        cleanup.map_err(|error| PluginHostError::Stop {
            package: self.package.clone(),
            message: diagnostic(&self.redactor, error, &stderr),
        })
    }
}

fn decode_result(value: Value) -> Result<PluginResult, String> {
    let object = value
        .as_object()
        .ok_or_else(|| "result was not an object".to_owned())?;
    let title = object
        .get("title")
        .and_then(Value::as_str)
        .ok_or_else(|| "result.title was not a string".to_owned())?
        .to_owned();
    let output = object
        .get("output")
        .and_then(Value::as_str)
        .ok_or_else(|| "result.output was not a string".to_owned())?
        .to_owned();
    let metadata = match object.get("metadata") {
        None | Some(Value::Null) => Map::new(),
        Some(Value::Object(metadata)) => metadata.clone(),
        Some(_) => return Err("result.metadata was not an object".to_owned()),
    };
    Ok(PluginResult {
        title,
        output,
        metadata,
    })
}

async fn exchange(state: &mut ProcessState, id: u64, request: Value) -> Result<Value, RpcFailure> {
    let mut encoded = serde_json::to_vec(&request).map_err(|error| RpcFailure::Protocol {
        message: error.to_string(),
    })?;
    encoded.push(b'\n');
    state
        .writer
        .write_all(&encoded)
        .await
        .map_err(RpcFailure::Io)?;
    state.writer.flush().await.map_err(RpcFailure::Io)?;
    let frame = read_frame(&mut state.reader).await?;
    let response: Value = serde_json::from_slice(&frame).map_err(|error| RpcFailure::Protocol {
        message: format!("malformed JSON-RPC response: {error}"),
    })?;
    if response.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(RpcFailure::Protocol {
            message: "response omitted `jsonrpc: \"2.0\"`".to_owned(),
        });
    }
    let Some(response_id) = response.get("id").and_then(Value::as_u64) else {
        return Err(RpcFailure::Protocol {
            message: "response omitted an integer id".to_owned(),
        });
    };
    if response_id != id {
        return Err(RpcFailure::Protocol {
            message: format!("response id {response_id} did not match request id {id}"),
        });
    }
    if let Some(error) = response.get("error") {
        let code = error.get("code").and_then(Value::as_i64).unwrap_or(-32_000);
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("plugin returned an unspecified JSON-RPC error");
        return Err(RpcFailure::Peer(format!("{message} (code {code})")));
    }
    response
        .get("result")
        .cloned()
        .ok_or_else(|| RpcFailure::Protocol {
            message: "response contained neither result nor error".to_owned(),
        })
}

async fn read_frame(reader: &mut (impl AsyncBufRead + Unpin)) -> Result<Vec<u8>, RpcFailure> {
    let mut frame = Vec::new();
    loop {
        let available = reader.fill_buf().await.map_err(RpcFailure::Io)?;
        if available.is_empty() {
            return Err(RpcFailure::Protocol {
                message: "plugin stdout ended before a JSON-RPC response".to_owned(),
            });
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let take = newline.map_or(available.len(), |index| index + 1);
        if frame.len().saturating_add(take) > MAX_FRAME_BYTES {
            return Err(RpcFailure::Protocol {
                message: format!("plugin response exceeded {MAX_FRAME_BYTES} bytes"),
            });
        }
        frame.extend_from_slice(&available[..take]);
        reader.consume(take);
        if newline.is_some() {
            while frame
                .last()
                .is_some_and(|byte| matches!(byte, b'\n' | b'\r'))
            {
                frame.pop();
            }
            return Ok(frame);
        }
    }
}

enum RpcFailure {
    Io(std::io::Error),
    Protocol { message: String },
    Peer(String),
    TimedOut,
    Cancelled,
}

impl RpcFailure {
    fn message(&self) -> String {
        match self {
            Self::Io(error) => format!("JSON-RPC transport failed: {error}"),
            Self::Protocol { message } => message.clone(),
            Self::Peer(message) => message.clone(),
            Self::TimedOut => "JSON-RPC request timed out".to_owned(),
            Self::Cancelled => "JSON-RPC request was cancelled".to_owned(),
        }
    }
}

async fn terminate(state: &mut ProcessState) -> Result<(), String> {
    if state
        .child
        .try_wait()
        .map_err(|error| error.to_string())?
        .is_some()
    {
        return Ok(());
    }
    if let Some(pid) = state.child.id() {
        zuno_process::request_contained_process_shutdown(pid).map_err(|error| error.to_string())?;
    }
    tokio::time::timeout(STOP_WAIT, state.child.wait())
        .await
        .map_err(|_| format!("process did not exit within {STOP_WAIT:?}"))?
        .map(|_| ())
        .map_err(|error| error.to_string())
}

async fn read_bounded(mut reader: impl AsyncRead + Unpin, limit: usize) -> String {
    let mut output = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                let remaining = limit.saturating_sub(output.len());
                if remaining > 0 {
                    output.extend_from_slice(&chunk[..read.min(remaining)]);
                }
            }
        }
    }
    String::from_utf8_lossy(&output).into_owned()
}

async fn finish_stderr(state: &mut ProcessState) -> String {
    let task = std::mem::replace(&mut state.stderr, tokio::spawn(async { String::new() }));
    tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default()
}

fn diagnostic(redactor: &SecretRedactor, message: impl AsRef<str>, stderr: &str) -> String {
    let message = redactor.safe(message);
    let stderr = redactor.safe(stderr.trim());
    if stderr.is_empty() {
        message
    } else {
        format!("{message}; stderr: {stderr}")
    }
}
