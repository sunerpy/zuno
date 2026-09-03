//! Native adapters for host-installed coding-agent products.
//!
//! The adapters deliberately use each product's supported non-interactive protocol:
//! Codex app-server JSON-RPC and Claude Code's stream-json print mode. They inherit the
//! user's native installation, configuration, authentication, working directory, and
//! process environment. Zuno never reads or copies either product's credentials.

use async_trait::async_trait;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::process::{Child, ChildStdin, Command};
use tokio_util::sync::CancellationToken;
use zuno_config::schema::product_agent::{
    ProductAgentConfig, ProductAgentKind, ProductAgentPermissionMode,
};
use zuno_paths::Env;

const STDERR_LIMIT: usize = 64 * 1024;
const CODEX_INITIALIZE_ID: i64 = 1;
const CODEX_THREAD_START_ID: i64 = 2;
const CODEX_TURN_START_ID: i64 = 3;
const CODEX_TURN_INTERRUPT_ID: i64 = 4;
const CODEX_LEGACY_THREAD_START_ID: i64 = 5;

/// One external product invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProductAgentRequest {
    /// The exact task text.
    pub prompt: String,
    /// Optional short label used only for diagnostics.
    pub description: Option<String>,
    /// Directory inherited by the native product.
    pub directory: PathBuf,
}

/// The final model-visible product result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProductAgentResult {
    /// Final answer only. Internal reasoning and tool streams are intentionally omitted.
    pub text: String,
}

/// A configured native product provider.
#[async_trait]
pub trait ProductAgent: Send + Sync {
    /// Which product this adapter drives.
    fn kind(&self) -> ProductAgentKind;

    /// Run one fresh, non-resumable product turn.
    async fn run(
        &self,
        request: ProductAgentRequest,
        cancellation: CancellationToken,
    ) -> Result<ProductAgentResult, ProductAgentError>;
}

/// Why a product-agent invocation did not produce a final answer.
#[derive(Debug, thiserror::Error)]
pub enum ProductAgentError {
    /// The process could not be started.
    #[error("{product} could not start: {message}")]
    Spawn {
        product: &'static str,
        message: String,
    },
    /// The installed product does not speak the required protocol.
    #[error("{product} protocol is incompatible: {message}")]
    Incompatible {
        product: &'static str,
        message: String,
    },
    /// The product completed with a typed failure.
    #[error("{product} failed: {message}")]
    Failed {
        product: &'static str,
        message: String,
    },
    /// Native permissions refused the requested operation.
    #[error("{product} permission denied: {message}")]
    Denied {
        product: &'static str,
        message: String,
    },
    /// The caller cancelled the invocation and the complete process tree was reaped.
    #[error("{product} invocation was cancelled")]
    Cancelled { product: &'static str },
    /// The process or protocol disappeared after work may already have happened.
    #[error("{product} outcome is uncertain: {message}")]
    Uncertain {
        product: &'static str,
        message: String,
    },
}

impl ProductAgentError {
    /// Whether authoritative state must be inspected before any retry.
    #[must_use]
    pub const fn is_uncertain(&self) -> bool {
        matches!(self, Self::Uncertain { .. })
    }
}

/// Build one native provider from validated configuration.
pub fn configured(
    instance: &str,
    config: &ProductAgentConfig,
    environment: &Env,
) -> Result<Arc<dyn ProductAgent>, String> {
    config.validate(instance)?;
    let environment = ChildEnvironment::new(environment, config.env.as_ref());
    let command = config.resolved_command().to_owned();
    let permission = config.resolved_permission_mode();
    Ok(match config.kind {
        ProductAgentKind::Codex => Arc::new(CodexAgent {
            command,
            permission,
            environment,
        }),
        ProductAgentKind::ClaudeCode => Arc::new(ClaudeCodeAgent {
            command,
            permission,
            environment,
        }),
    })
}

#[derive(Clone)]
struct ChildEnvironment {
    values: BTreeMap<String, String>,
    secrets: Vec<String>,
}

impl ChildEnvironment {
    fn new(base: &Env, overlay: Option<&BTreeMap<String, String>>) -> Self {
        let mut values = base
            .iter()
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
            .collect::<BTreeMap<_, _>>();
        if let Some(overlay) = overlay {
            values.extend(overlay.clone());
        }
        let secrets = values
            .iter()
            .filter(|(key, value)| is_sensitive_name(key) && !value.is_empty())
            .map(|(_key, value)| value.clone())
            .collect();
        Self { values, secrets }
    }

    fn apply(&self, command: &mut Command) {
        command.envs(&self.values);
    }

    fn safe(&self, value: impl AsRef<str>) -> String {
        redact(value.as_ref(), &self.secrets)
    }
}

fn is_sensitive_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    ["KEY", "TOKEN", "SECRET", "PASSWORD", "CREDENTIAL"]
        .iter()
        .any(|marker| upper.contains(marker))
}

fn redact(value: &str, secrets: &[String]) -> String {
    let mut safe = value.to_owned();
    for secret in secrets {
        if secret.len() >= 4 {
            safe = safe.replace(secret, "[REDACTED]");
        }
    }
    safe.lines()
        .map(|line| {
            let lower = line.to_ascii_lowercase();
            if lower.contains("authorization: bearer ")
                || lower.contains("api_key=")
                || lower.contains("apikey=")
            {
                "[REDACTED CREDENTIAL LINE]".to_owned()
            } else {
                line.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

struct CodexAgent {
    command: String,
    permission: ProductAgentPermissionMode,
    environment: ChildEnvironment,
}

#[async_trait]
impl ProductAgent for CodexAgent {
    fn kind(&self) -> ProductAgentKind {
        ProductAgentKind::Codex
    }

    async fn run(
        &self,
        request: ProductAgentRequest,
        cancellation: CancellationToken,
    ) -> Result<ProductAgentResult, ProductAgentError> {
        run_codex(self, request, cancellation).await
    }
}

async fn run_codex(
    agent: &CodexAgent,
    request: ProductAgentRequest,
    cancellation: CancellationToken,
) -> Result<ProductAgentResult, ProductAgentError> {
    let args = [OsString::from("app-server"), OsString::from("--stdio")];
    let (program, guarded_args) = zuno_process::guarded_argv(&agent.command, &args);
    let mut command = contained_command(program, guarded_args, &request.directory);
    agent.environment.apply(&mut command);
    let mut child = command.spawn().map_err(|error| ProductAgentError::Spawn {
        product: "Codex",
        message: agent.environment.safe(error.to_string()),
    })?;
    let stdin = child.stdin.take().ok_or_else(|| ProductAgentError::Spawn {
        product: "Codex",
        message: "stdio guard did not expose stdin".to_owned(),
    })?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ProductAgentError::Spawn {
            product: "Codex",
            message: "stdio guard did not expose stdout".to_owned(),
        })?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| ProductAgentError::Spawn {
            product: "Codex",
            message: "stdio guard did not expose stderr".to_owned(),
        })?;
    let stderr_task = tokio::spawn(read_bounded(stderr, STDERR_LIMIT));
    let mut writer = BufWriter::new(stdin);
    let mut reader = BufReader::new(stdout);

    send_rpc(
        &mut writer,
        json!({
            "id": CODEX_INITIALIZE_ID,
            "method": "initialize",
            "params": {
                "clientInfo": {
                    "name": "zuno",
                    "title": "Zuno product subagent",
                    "version": env!("CARGO_PKG_VERSION")
                },
                "capabilities": {
                    "experimentalApi": true
                }
            }
        }),
    )
    .await
    .map_err(|error| incompatible("Codex", &agent.environment, error))?;
    if let Err(error) = wait_for_response(
        &mut reader,
        &mut writer,
        CODEX_INITIALIZE_ID,
        false,
        &cancellation,
    )
    .await
    {
        return Err(handshake_failure("Codex", &mut child, &agent.environment, error).await);
    }
    send_rpc(&mut writer, json!({"method":"initialized","params":{}}))
        .await
        .map_err(|error| incompatible("Codex", &agent.environment, error))?;

    let thread = match start_codex_thread(
        &mut reader,
        &mut writer,
        CODEX_THREAD_START_ID,
        &request.directory,
        agent.permission,
        CodexProtocolDialect::Current,
        &cancellation,
    )
    .await
    {
        Ok(thread) => thread,
        Err(error) if error.invalid_params => {
            match start_codex_thread(
                &mut reader,
                &mut writer,
                CODEX_LEGACY_THREAD_START_ID,
                &request.directory,
                agent.permission,
                CodexProtocolDialect::Legacy,
                &cancellation,
            )
            .await
            {
                Ok(thread) => thread,
                Err(error) => {
                    return Err(
                        handshake_failure("Codex", &mut child, &agent.environment, error).await,
                    );
                }
            }
        }
        Err(error) => {
            return Err(handshake_failure("Codex", &mut child, &agent.environment, error).await);
        }
    };
    let thread_id = thread
        .pointer("/result/thread/id")
        .and_then(Value::as_str)
        .ok_or_else(|| ProductAgentError::Incompatible {
            product: "Codex",
            message: "thread/start response did not contain result.thread.id".to_owned(),
        })?
        .to_owned();

    send_rpc(
        &mut writer,
        json!({
            "id": CODEX_TURN_START_ID,
            "method": "turn/start",
            "params": {
                "threadId": thread_id,
                "input": [{"type":"text","text":request.prompt}]
            }
        }),
    )
    .await
    .map_err(|error| uncertain("Codex", &agent.environment, error))?;
    let turn = match wait_for_response(
        &mut reader,
        &mut writer,
        CODEX_TURN_START_ID,
        agent.permission.is_dangerous(),
        &cancellation,
    )
    .await
    {
        Ok(turn) => turn,
        Err(error) => {
            return Err(turn_start_failure("Codex", &mut child, &agent.environment, error).await);
        }
    };
    let turn_id = turn
        .pointer("/result/turn/id")
        .and_then(Value::as_str)
        .ok_or_else(|| ProductAgentError::Uncertain {
            product: "Codex",
            message: "turn/start response did not contain result.turn.id".to_owned(),
        })?
        .to_owned();

    let mut final_text = String::new();
    loop {
        let message = match read_rpc(&mut reader, &cancellation).await {
            Ok(message) => message,
            Err(ReadRpcError::Cancelled) => {
                let _ignored = send_rpc(
                    &mut writer,
                    json!({
                        "id": CODEX_TURN_INTERRUPT_ID,
                        "method": "turn/interrupt",
                        "params": {"threadId":thread_id,"turnId":turn_id}
                    }),
                )
                .await;
                terminate(&mut child).await;
                let _stderr = stderr_task.await;
                return Err(ProductAgentError::Cancelled { product: "Codex" });
            }
            Err(ReadRpcError::Io(error)) => {
                terminate(&mut child).await;
                let stderr = finish_stderr(stderr_task).await;
                return Err(ProductAgentError::Uncertain {
                    product: "Codex",
                    message: diagnostic(
                        &agent.environment,
                        format!("app-server stream ended: {error}"),
                        &stderr,
                    ),
                });
            }
            Err(ReadRpcError::Malformed(error)) => {
                terminate(&mut child).await;
                let stderr = finish_stderr(stderr_task).await;
                return Err(ProductAgentError::Uncertain {
                    product: "Codex",
                    message: diagnostic(&agent.environment, error, &stderr),
                });
            }
        };
        if is_server_request(&message) {
            if let Err(error) =
                respond_to_server_request(&mut writer, &message, agent.permission.is_dangerous())
                    .await
            {
                terminate(&mut child).await;
                let _stderr = stderr_task.await;
                return Err(uncertain("Codex", &agent.environment, error));
            }
            continue;
        }
        match message.get("method").and_then(Value::as_str) {
            Some("item/completed") => {
                if let Some(text) = message.pointer("/params/item").and_then(agent_message_text) {
                    final_text = text.to_owned();
                }
            }
            Some("turn/completed") => {
                let notification_turn = message.pointer("/params/turn");
                if let Some(text) = notification_turn
                    .and_then(|turn| turn.get("items"))
                    .and_then(Value::as_array)
                    .and_then(|items| items.iter().rev().find_map(agent_message_text))
                {
                    final_text = text.to_owned();
                }
                let status = notification_turn
                    .and_then(|turn| turn.get("status"))
                    .and_then(Value::as_str)
                    .unwrap_or("failed");
                let error = notification_turn
                    .and_then(|turn| turn.get("error"))
                    .filter(|error| !error.is_null())
                    .map(ToString::to_string);
                terminate(&mut child).await;
                let stderr = finish_stderr(stderr_task).await;
                return match status {
                    "completed" if !final_text.trim().is_empty() => {
                        Ok(ProductAgentResult { text: final_text })
                    }
                    "interrupted" => Err(ProductAgentError::Cancelled { product: "Codex" }),
                    _ => {
                        let message = diagnostic(
                            &agent.environment,
                            error.unwrap_or_else(|| format!("turn ended with status `{status}`")),
                            &stderr,
                        );
                        if is_permission_denial(&message) {
                            Err(ProductAgentError::Denied {
                                product: "Codex",
                                message,
                            })
                        } else {
                            Err(ProductAgentError::Failed {
                                product: "Codex",
                                message,
                            })
                        }
                    }
                };
            }
            _ => {}
        }
    }
}

fn agent_message_text(item: &Value) -> Option<&str> {
    (item.get("type").and_then(Value::as_str) == Some("agentMessage"))
        .then(|| item.get("text").and_then(Value::as_str))
        .flatten()
}

struct ClaudeCodeAgent {
    command: String,
    permission: ProductAgentPermissionMode,
    environment: ChildEnvironment,
}

#[async_trait]
impl ProductAgent for ClaudeCodeAgent {
    fn kind(&self) -> ProductAgentKind {
        ProductAgentKind::ClaudeCode
    }

    async fn run(
        &self,
        request: ProductAgentRequest,
        cancellation: CancellationToken,
    ) -> Result<ProductAgentResult, ProductAgentError> {
        run_claude_code(self, request, cancellation).await
    }
}

async fn run_claude_code(
    agent: &ClaudeCodeAgent,
    request: ProductAgentRequest,
    cancellation: CancellationToken,
) -> Result<ProductAgentResult, ProductAgentError> {
    let permission = agent
        .permission
        .as_claude_code()
        .expect("validated Claude Code permission");
    let disallowed = if agent.permission == ProductAgentPermissionMode::Plan {
        "AskUserQuestion,ExitPlanMode"
    } else {
        "AskUserQuestion"
    };
    let mut args = vec![
        OsString::from("--print"),
        OsString::from("--verbose"),
        OsString::from("--output-format"),
        OsString::from("stream-json"),
        OsString::from("--no-session-persistence"),
        OsString::from("--permission-mode"),
        OsString::from(permission),
        OsString::from("--disallowedTools"),
        OsString::from(disallowed),
    ];
    if agent.permission == ProductAgentPermissionMode::BypassPermissions {
        args.push(OsString::from("--dangerously-skip-permissions"));
    }
    // The prompt is model-generated untrusted text, so it is passed as an operand rather than
    // left where the product's own CLI parser can read it as options. Without this terminator a
    // prompt beginning with `-` is parsed by the child: `--dangerously-skip-permissions`,
    // `--mcp-config`, or `--resume` in the first characters of a prompt would each be honoured,
    // which turns prompt text into a privilege and configuration decision. Every argument Zuno
    // means as an option is already pushed above.
    args.push(OsString::from("--"));
    args.push(OsString::from(request.prompt));
    let (program, guarded_args) = zuno_process::guarded_argv(&agent.command, &args);
    let mut command = contained_command(program, guarded_args, &request.directory);
    agent.environment.apply(&mut command);
    let mut child = command.spawn().map_err(|error| ProductAgentError::Spawn {
        product: "Claude Code",
        message: agent.environment.safe(error.to_string()),
    })?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ProductAgentError::Spawn {
            product: "Claude Code",
            message: "stdio guard did not expose stdout".to_owned(),
        })?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| ProductAgentError::Spawn {
            product: "Claude Code",
            message: "stdio guard did not expose stderr".to_owned(),
        })?;
    let stderr_task = tokio::spawn(read_bounded(stderr, STDERR_LIMIT));
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    loop {
        line.clear();
        let read = tokio::select! {
            () = cancellation.cancelled() => {
                terminate(&mut child).await;
                let _stderr = stderr_task.await;
                return Err(ProductAgentError::Cancelled { product: "Claude Code" });
            }
            read = reader.read_line(&mut line) => read
        };
        let read = match read {
            Ok(read) => read,
            Err(error) => {
                terminate(&mut child).await;
                let stderr = finish_stderr(stderr_task).await;
                return Err(ProductAgentError::Uncertain {
                    product: "Claude Code",
                    message: diagnostic(
                        &agent.environment,
                        format!("stream-json read failed: {error}"),
                        &stderr,
                    ),
                });
            }
        };
        if read == 0 {
            let status = child.wait().await.ok();
            let stderr = finish_stderr(stderr_task).await;
            return Err(ProductAgentError::Uncertain {
                product: "Claude Code",
                message: diagnostic(
                    &agent.environment,
                    format!("stream-json ended before a result (status {status:?})"),
                    &stderr,
                ),
            });
        }
        let message: Value = match serde_json::from_str(line.trim_end()) {
            Ok(message) => message,
            Err(error) => {
                terminate(&mut child).await;
                let stderr = finish_stderr(stderr_task).await;
                return Err(ProductAgentError::Uncertain {
                    product: "Claude Code",
                    message: diagnostic(
                        &agent.environment,
                        format!("malformed stream-json message: {error}"),
                        &stderr,
                    ),
                });
            }
        };
        if message.get("type").and_then(Value::as_str) != Some("result") {
            continue;
        }
        let is_error = message
            .get("is_error")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let result = message
            .get("result")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let subtype = message
            .get("subtype")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        terminate(&mut child).await;
        let stderr = finish_stderr(stderr_task).await;
        if !is_error && !result.trim().is_empty() {
            return Ok(ProductAgentResult { text: result });
        }
        let message = diagnostic(
            &agent.environment,
            if result.is_empty() {
                format!("result subtype `{subtype}`")
            } else {
                result
            },
            &stderr,
        );
        if is_permission_denial(&message) {
            return Err(ProductAgentError::Denied {
                product: "Claude Code",
                message,
            });
        }
        return Err(ProductAgentError::Failed {
            product: "Claude Code",
            message,
        });
    }
}

fn is_permission_denial(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("permission") || message.contains("denied")
}

fn contained_command(program: OsString, arguments: Vec<OsString>, directory: &Path) -> Command {
    let mut command = Command::new(program);
    command
        .args(arguments)
        .current_dir(directory)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    command
}

async fn send_rpc(writer: &mut BufWriter<ChildStdin>, value: Value) -> std::io::Result<()> {
    let mut encoded = serde_json::to_vec(&value).map_err(std::io::Error::other)?;
    encoded.push(b'\n');
    writer.write_all(&encoded).await?;
    writer.flush().await
}

enum ReadRpcError {
    Cancelled,
    Io(std::io::Error),
    Malformed(String),
}

#[derive(Debug)]
struct RpcResponseError {
    message: String,
    invalid_params: bool,
    /// The caller cancelled while this request was outstanding.
    ///
    /// Typed rather than recovered from `message`: retry and pause decisions read typed errors,
    /// and a rendered string cannot distinguish a user interruption from a protocol failure.
    cancelled: bool,
}

impl RpcResponseError {
    fn transport(message: impl ToString) -> Self {
        Self {
            message: message.to_string(),
            invalid_params: false,
            cancelled: false,
        }
    }

    /// The caller cancelled before the response arrived.
    ///
    /// `invalid_params` stays false, so a cancellation can never be mistaken for the protocol
    /// rejection that selects the legacy `thread/start` dialect.
    fn cancelled() -> Self {
        Self {
            message: "invocation was cancelled".to_owned(),
            invalid_params: false,
            cancelled: true,
        }
    }
}

impl std::fmt::Display for RpcResponseError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

#[derive(Clone, Copy)]
enum CodexProtocolDialect {
    Current,
    Legacy,
}

impl CodexProtocolDialect {
    fn approval_policy(self, permission: ProductAgentPermissionMode) -> &'static str {
        let current = permission
            .as_codex_approval_policy()
            .expect("validated Codex permission");
        match (self, current) {
            (Self::Legacy, "unlessTrusted") => "untrusted",
            (Self::Legacy, "onRequest") => "on-request",
            _ => current,
        }
    }

    const fn sandbox(self, permission: ProductAgentPermissionMode) -> &'static str {
        match (self, permission.is_dangerous()) {
            (Self::Current, true) => "dangerFullAccess",
            (Self::Current, false) => "workspaceWrite",
            (Self::Legacy, true) => "danger-full-access",
            (Self::Legacy, false) => "workspace-write",
        }
    }
}

async fn start_codex_thread<R>(
    reader: &mut BufReader<R>,
    writer: &mut BufWriter<ChildStdin>,
    id: i64,
    directory: &Path,
    permission: ProductAgentPermissionMode,
    dialect: CodexProtocolDialect,
    cancellation: &CancellationToken,
) -> Result<Value, RpcResponseError>
where
    R: AsyncRead + Unpin,
{
    send_rpc(
        writer,
        json!({
            "id": id,
            "method": "thread/start",
            "params": {
                "cwd": directory,
                "ephemeral": true,
                "approvalPolicy": dialect.approval_policy(permission),
                "sandbox": dialect.sandbox(permission)
            }
        }),
    )
    .await
    .map_err(RpcResponseError::transport)?;
    wait_for_response(reader, writer, id, permission.is_dangerous(), cancellation).await
}

async fn read_rpc<R>(
    reader: &mut BufReader<R>,
    cancellation: &CancellationToken,
) -> Result<Value, ReadRpcError>
where
    R: AsyncRead + Unpin,
{
    let mut line = String::new();
    let read = tokio::select! {
        () = cancellation.cancelled() => return Err(ReadRpcError::Cancelled),
        read = reader.read_line(&mut line) => read.map_err(ReadRpcError::Io)?
    };
    if read == 0 {
        return Err(ReadRpcError::Io(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "app-server stdout closed",
        )));
    }
    serde_json::from_str(line.trim_end())
        .map_err(|error| ReadRpcError::Malformed(format!("malformed JSON-RPC message: {error}")))
}

async fn wait_for_response<R>(
    reader: &mut BufReader<R>,
    writer: &mut BufWriter<ChildStdin>,
    id: i64,
    approve: bool,
    cancellation: &CancellationToken,
) -> Result<Value, RpcResponseError>
where
    R: AsyncRead + Unpin,
{
    loop {
        let message = match read_rpc(reader, cancellation).await {
            Ok(message) => message,
            Err(ReadRpcError::Cancelled) => return Err(RpcResponseError::cancelled()),
            Err(ReadRpcError::Io(error)) => return Err(RpcResponseError::transport(error)),
            Err(ReadRpcError::Malformed(error)) => {
                return Err(RpcResponseError::transport(error));
            }
        };
        if is_server_request(&message) {
            respond_to_server_request(writer, &message, approve)
                .await
                .map_err(RpcResponseError::transport)?;
            continue;
        }
        if message.get("id").and_then(Value::as_i64) != Some(id) {
            continue;
        }
        if let Some(error) = message.get("error") {
            return Err(RpcResponseError {
                message: format!("request {id} failed: {error}"),
                invalid_params: error.get("code").and_then(Value::as_i64) == Some(-32602),
                cancelled: false,
            });
        }
        return Ok(message);
    }
}

fn is_server_request(message: &Value) -> bool {
    message.get("id").is_some()
        && message.get("method").and_then(Value::as_str).is_some()
        && message.get("result").is_none()
        && message.get("error").is_none()
}

async fn respond_to_server_request(
    writer: &mut BufWriter<ChildStdin>,
    request: &Value,
    approve: bool,
) -> std::io::Result<()> {
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let method = request
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let result = match method {
        "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
            json!({"decision": if approve {"accept"} else {"decline"}})
        }
        "item/tool/requestUserInput" => json!({"answers":{}}),
        "item/permissions/requestApproval" => json!({
            "permissions": if approve {
                request
                    .pointer("/params/permissions")
                    .cloned()
                    .unwrap_or_else(|| json!({}))
            } else {
                json!({})
            },
            "scope": "turn"
        }),
        "mcpServer/elicitation/create" => json!({"action":"decline","content":null}),
        "currentTime/read" => json!({
            "currentTimeAt": SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
        }),
        _ => {
            return send_rpc(
                writer,
                json!({
                    "id":id,
                    "error":{"code":-32601,"message":"Zuno product subagent does not implement this request"}
                }),
            )
            .await;
        }
    };
    send_rpc(writer, json!({"id":id,"result":result})).await
}

async fn read_bounded(mut reader: impl AsyncRead + Unpin, limit: usize) -> String {
    let mut result = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                let remaining = limit.saturating_sub(result.len());
                if remaining > 0 {
                    result.extend_from_slice(&chunk[..read.min(remaining)]);
                }
            }
        }
    }
    String::from_utf8_lossy(&result).into_owned()
}

async fn finish_stderr(task: tokio::task::JoinHandle<String>) -> String {
    tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default()
}

async fn terminate(child: &mut Child) {
    if child.try_wait().ok().flatten().is_none()
        && let Some(pid) = child.id()
    {
        let _ignored = zuno_process::request_contained_process_shutdown(pid);
    }
    let _ignored = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
}

fn diagnostic(environment: &ChildEnvironment, message: String, stderr: &str) -> String {
    let message = environment.safe(message);
    let stderr = environment.safe(stderr.trim());
    if stderr.is_empty() {
        message
    } else {
        format!("{message}; stderr: {stderr}")
    }
}

fn incompatible(
    product: &'static str,
    environment: &ChildEnvironment,
    message: impl std::fmt::Display,
) -> ProductAgentError {
    ProductAgentError::Incompatible {
        product,
        message: environment.safe(message.to_string()),
    }
}

fn uncertain(
    product: &'static str,
    environment: &ChildEnvironment,
    message: impl std::fmt::Display,
) -> ProductAgentError {
    ProductAgentError::Uncertain {
        product,
        message: environment.safe(message.to_string()),
    }
}

/// Reap the tree and classify a failure raised before `turn/start` was written.
///
/// `initialize`, `initialized`, and `thread/start` request nothing of the user's workspace: an
/// ephemeral thread runs no tools and writes no files, so at this phase a cancellation is a plain
/// user interruption and nothing about the outcome is unknown. Reporting it as
/// [`ProductAgentError::Incompatible`] would tell recovery that the installed product cannot speak
/// the protocol, which blocks the goal permanently instead of pausing it, and would tell the user
/// their Codex installation is broken because they pressed cancel. Any other failure at this phase
/// really is a protocol incompatibility.
async fn handshake_failure(
    product: &'static str,
    child: &mut Child,
    environment: &ChildEnvironment,
    error: RpcResponseError,
) -> ProductAgentError {
    terminate(child).await;
    if error.cancelled {
        ProductAgentError::Cancelled { product }
    } else {
        incompatible(product, environment, error)
    }
}

/// Reap the tree and classify a failure raised while the `turn/start` response is outstanding.
///
/// This is the one phase where a cancellation stays uncertain, and the line is drawn by evidence
/// rather than by intent. `turn/start` has already been flushed to the product, so it may already
/// be running commands and editing files in the user's directory, and Zuno holds no turn id with
/// which to address `turn/interrupt` and no stream event describing what ran. A lost response
/// around a side effect is an uncertain outcome: it must be persisted for authoritative-state
/// inspection and never mechanically replayed. Once the turn id is known the streaming loop
/// interrupts that exact turn and reports [`ProductAgentError::Cancelled`], because there the
/// outcome is observed rather than unknown.
async fn turn_start_failure(
    product: &'static str,
    child: &mut Child,
    environment: &ChildEnvironment,
    error: RpcResponseError,
) -> ProductAgentError {
    terminate(child).await;
    if error.cancelled {
        return ProductAgentError::Uncertain {
            product,
            message: "invocation was cancelled while the turn/start response was outstanding; \
                      the turn may already have started and changed the working directory"
                .to_owned(),
        };
    }
    uncertain(product, environment, error)
}
