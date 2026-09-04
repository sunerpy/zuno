use std::ffi::OsString;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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
/// Ceiling on one blocking process-control call dispatched off the runtime worker.
///
/// A fixed constant, never derived from a plugin manifest or a peer response: exceeding it
/// reports a stop that was not confirmed, which only ever adds uncertainty to the outcome.
const PROCESS_CONTROL_LIMIT: Duration = Duration::from_secs(2);

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
        root_literal: spec.root_literal,
        workspace_literal: spec.workspace_literal,
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
    // The package directory and workspace exactly as the JSON protocol carries them,
    // validated when the runtime surface was built so `initialize` has no path left to
    // reject, mangle, or silently substitute.
    root_literal: String,
    workspace_literal: String,
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
                    "packageRoot": self.root_literal,
                    "workspace": self.workspace_literal,
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
        let dispatched = AtomicBool::new(false);
        let operation = exchange(state, id, request, &dispatched);
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
                let cleanup = terminate(&mut failed.child).await;
                let stderr = finish_stderr(&mut failed).await;
                match failure {
                    // The dispatch fact is reported as observed, not filtered through
                    // `uncertain_after_send`: only a call whose dispatch matters is given
                    // an interrupt to be cancelled by, and folding a policy flag into an
                    // observation is how the report came to describe a dispatch that did
                    // not happen. Dropping it here can only widen uncertainty.
                    RpcFailure::Cancelled => {
                        Err(self.cancelled(dispatched.load(Ordering::Acquire), cleanup, &stderr))
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

    /// The cancellation outcome of a call whose host has just been retired.
    ///
    /// Two independent facts decide it. `dispatched` says whether the plugin owned the
    /// call: killing the process group settles this host, never a side effect the plugin
    /// had already begun, because no reply survives to say how far it got. `cleanup` says
    /// whether the plugin is actually stopped — a failed stop (`taskkill /f /t` exiting
    /// non-zero against a live tree, a process that outlived `STOP_WAIT`) leaves a process
    /// that may still be acting on the call, so the outcome is undecided even when the
    /// request never reached its stdin. Neither fact may be merged into the other: a
    /// cancellation reported as decided is published to the model and the durable record as
    /// cleanup that completed, and one reported as dispatched when it was not is published
    /// as a side effect that may be half applied. Both travel to the report separately.
    fn cancelled(
        &self,
        dispatched: bool,
        cleanup: Result<(), String>,
        stderr: &str,
    ) -> PluginHostError {
        let cleanup = cleanup.err().map(|error| {
            diagnostic(
                &self.redactor,
                format!("stopping the plugin failed: {error}"),
                stderr,
            )
        });
        PluginHostError::Cancelled {
            package: self.package.clone(),
            dispatched,
            cleanup,
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
        let cleanup = terminate(&mut state.child).await;
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
            let dispatched = AtomicBool::new(false);
            let _ignored =
                tokio::time::timeout(self.timeout, exchange(&mut state, id, request, &dispatched))
                    .await;
            state
        };
        let cleanup = terminate(&mut state.child).await;
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

/// Write one request and read its response, recording when the plugin owns the call.
///
/// `dispatched` is set exactly when the frame has reached the plugin's stdin, which is
/// the point after which a cancellation can no longer claim that nothing ran.
async fn exchange(
    state: &mut ProcessState,
    id: u64,
    request: Value,
    dispatched: &AtomicBool,
) -> Result<Value, RpcFailure> {
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
    dispatched.store(true, Ordering::Release);
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

async fn terminate(child: &mut Child) -> Result<(), String> {
    terminate_with(child, zuno_process::request_contained_process_shutdown).await
}

/// Stop one plugin process tree through an injected process-control call.
///
/// `shutdown` is a parameter only so a test can drive this exact function with a call that
/// really does block: on a Unix host the production call is a bare `kill(2)` that returns
/// immediately, so nothing here could otherwise show that the dispatch leaves the runtime
/// worker, and a test that called the dispatch helper directly would keep passing after
/// this function was changed back to an inline call.
async fn terminate_with<F>(child: &mut Child, shutdown: F) -> Result<(), String>
where
    F: FnOnce(u32) -> std::io::Result<()> + Send + 'static,
{
    if child
        .try_wait()
        .map_err(|error| error.to_string())?
        .is_some()
    {
        return Ok(());
    }
    if let Some(pid) = child.id() {
        // `zuno_process::request_contained_process_shutdown` keeps a blocking signature
        // because synchronous callers need it, and on Unix it is a bare `kill(2)`. On
        // Windows the same call spawns `taskkill /pid n /f /t` and waits for the whole
        // tree walk, and every session runtime Zuno builds is current-thread: an inline
        // call would freeze the provider stream and the client event pump until the walk
        // finished, which is precisely when a cancelling user needs them draining.
        match off_runtime_worker(move || shutdown(pid)).await {
            Some(Ok(())) => {}
            Some(Err(error)) => return Err(error.to_string()),
            None => {
                return Err(format!(
                    "stop request did not return within {PROCESS_CONTROL_LIMIT:?}"
                ));
            }
        }
    }
    tokio::time::timeout(STOP_WAIT, child.wait())
        .await
        .map_err(|_| format!("process did not exit within {STOP_WAIT:?}"))?
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// Run one blocking process-control call without stalling the runtime worker.
///
/// The join is bounded because moving the call off the worker does not make it finish: a
/// wedged `taskkill` is the Windows case this dispatch exists for, and an unbounded join
/// would hold the caller past `STOP_WAIT`. `None` therefore means the stop was not
/// confirmed, which is the honest outcome — it can only add uncertainty to what the caller
/// reports, never claim more than the inline call could. Aborting is not attempted: a
/// blocking task that has started cannot be cancelled, so the ceiling releases this await,
/// not the operating-system call, and the child is spawned with `kill_on_drop` so dropping
/// it still terminates the direct child.
async fn off_runtime_worker<T>(operation: impl FnOnce() -> T + Send + 'static) -> Option<T>
where
    T: Send + 'static,
{
    tokio::time::timeout(
        PROCESS_CONTROL_LIMIT,
        tokio::task::spawn_blocking(operation),
    )
    .await
    .ok()
    .and_then(Result::ok)
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

#[cfg(test)]
mod tests {
    // Used only by the Unix-gated tick-counting test; on Windows the import would be
    // reported as unused under `-D warnings`.
    #[cfg(unix)]
    use std::sync::atomic::AtomicUsize;

    use super::*;

    fn retired_host() -> ProcessPluginHost {
        ProcessPluginHost {
            package: "review-kit".to_owned(),
            root_literal: "/tmp/review-kit".to_owned(),
            workspace_literal: "/tmp/workspace".to_owned(),
            timeout: Duration::from_secs(5),
            capabilities: Vec::new(),
            redactor: SecretRedactor::from_process(),
            state: Mutex::new(None),
        }
    }

    /// A stop that was not confirmed leaves the cancellation undecided, and keeps the
    /// dispatch observation the report needs to describe it truthfully.
    ///
    /// The Windows case is `taskkill /pid n /f /t` exiting non-zero against a tree that is
    /// still alive: `terminate` returns immediately, well inside the dispatcher's grace
    /// window, so nothing else would ever contradict a decided verdict. Reporting one would
    /// publish "acknowledged cancellation and completed its cleanup" for a plugin that is
    /// still running and may still be writing. Merging the two facts into one bit instead
    /// fixes that verdict and then publishes the opposite lie — a dispatch this host
    /// observed did not happen — so this drives the exact input through both production
    /// functions: the host's own constructor, then the report the tool returns.
    #[test]
    fn a_failed_stop_makes_a_cancellation_undecided() {
        let host = retired_host();
        let error = host.cancelled(
            false,
            Err("taskkill failed for process tree 4242 with status exit code: 128".to_owned()),
            "",
        );
        let PluginHostError::Cancelled {
            dispatched,
            cleanup,
            ..
        } = &error
        else {
            panic!("a cancellation stays a cancellation: {error}");
        };
        assert!(!*dispatched, "the request never reached stdin: {error}");
        let cleanup = cleanup.as_deref().expect("the failed stop is reported");
        assert!(cleanup.contains("taskkill failed"), "{cleanup}");
        assert!(error.is_uncertain(), "{error}");

        let settled = error
            .cancellation_report("review_outline")
            .expect("a cancellation settles as a report");
        let claim = &settled.metadata[crate::host::METADATA_CANCELLATION_KEY];
        assert_eq!(claim["uncertain"], json!(true), "{claim}");
        assert_eq!(claim["dispatched"], json!(false), "{claim}");
        assert!(
            !settled.output.contains("had already been sent"),
            "{}",
            settled.output
        );
        assert!(
            settled.output.contains("may still be running"),
            "{}",
            settled.output
        );
    }

    #[test]
    fn a_confirmed_stop_reports_the_dispatch_state_it_observed() {
        let host = retired_host();
        for observed in [false, true] {
            let error = host.cancelled(observed, Ok(()), "");
            let PluginHostError::Cancelled {
                dispatched,
                cleanup,
                ..
            } = &error
            else {
                panic!("a cancellation stays a cancellation: {error}");
            };
            assert_eq!(*dispatched, observed, "{error}");
            assert_eq!(error.is_uncertain(), observed, "{error}");
            assert!(cleanup.is_none(), "{error}");
        }
    }

    /// Stopping a plugin must not freeze the session runtime it was cancelled from.
    ///
    /// Every session runtime Zuno builds is current-thread, so an inline blocking stop
    /// starves every other task on it — the provider stream and the client event pump
    /// included — for as long as the call takes. On Unix the production call is a bare
    /// `kill(2)`, so the injected stop is what a Windows `taskkill /f /t` tree walk does.
    #[cfg(unix)]
    #[tokio::test]
    async fn stopping_a_plugin_leaves_the_session_worker_free() {
        let mut child = Command::new("sleep")
            .arg("60")
            .kill_on_drop(true)
            .spawn()
            .expect("a live child to stop");
        let ticks = Arc::new(AtomicUsize::new(0));
        let pump = tokio::spawn({
            let ticks = Arc::clone(&ticks);
            async move {
                loop {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    ticks.fetch_add(1, Ordering::Release);
                }
            }
        });

        let cleanup = terminate_with(&mut child, |pid| {
            std::thread::sleep(Duration::from_millis(300));
            zuno_process::request_contained_process_shutdown(pid)
        })
        .await;

        pump.abort();
        assert_eq!(cleanup, Ok(()), "the child was reaped");
        let ticks = ticks.load(Ordering::Acquire);
        assert!(
            ticks >= 5,
            "the runtime worker was blocked for the whole stop: {ticks} ticks"
        );
    }
}
