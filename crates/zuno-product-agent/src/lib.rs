//! Native adapters for host-installed coding-agent products.
//!
//! The adapters deliberately use each product's supported non-interactive protocol:
//! Codex app-server JSON-RPC and Claude Code's stream-json print mode. They inherit the
//! user's native installation, configuration, authentication, working directory, and
//! process environment. Zuno never reads or copies either product's credentials.
//!
//! # Settlement
//!
//! Every exit from `run` is bounded, because an invocation that never settles never releases the
//! tool call that owns it and the cancellation the user asked for never completes. Reads from the
//! product select against the cancellation token; every await that a wedged product could otherwise
//! hold forever carries its own ceiling instead: `RPC_WRITE_LIMIT` on writing one JSON-RPC line into
//! its stdin, `PROCESS_CONTROL_LIMIT` on the blocking process-control call that reaps its tree,
//! `CHILD_REAP_LIMIT` on collecting the reaped child, and `STDERR_DRAIN_LIMIT` on draining its
//! stderr. A cancellation therefore costs at most the interrupt write plus the reap plus the drain,
//! and never an unbounded wait. Every ceiling is a fixed constant: none is derived from anything the
//! product, the model, or the workspace supplies, so no input can widen one.
//!
//! # Failure classification
//!
//! A native permission refusal is read from each protocol's own typed field, never from text.
//! Failure text, captured stderr, and model prose describe whatever the turn was doing, so letting
//! any of them choose the label would let a delegated task or repository contents decide that the
//! host's permissions had to be widened. When the typed field is absent or unrecognised the outcome
//! is the plain [`ProductAgentError::Failed`], which still carries the product's own failure text
//! verbatim.
//!
//! A refusal record is not an outcome. Both products record refused tool calls for the whole turn,
//! so the verdict additionally requires the product's own statement that the turn failed — Codex
//! `turn.status: "failed"`, Claude Code `is_error` — and that statement is read from the same value
//! that selects the branch it labels. Otherwise a turn the product reported as successful is labelled
//! a permission failure because its answer happened to be blank, which is how a delegated turn would
//! reach the `Denied` label with nothing in it a caller could check.

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
/// Ceiling on draining one child's stderr after its tree has been reaped.
const STDERR_DRAIN_LIMIT: Duration = Duration::from_secs(1);
/// Ceiling on waiting for one reaped child to be collected.
const CHILD_REAP_LIMIT: Duration = Duration::from_secs(2);
/// Ceiling on one blocking process-control call, which on Windows is a `taskkill /f /t` tree walk.
const PROCESS_CONTROL_LIMIT: Duration = Duration::from_secs(2);
/// Ceiling on writing one JSON-RPC line into the product's stdin.
const RPC_WRITE_LIMIT: Duration = Duration::from_secs(5);
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
    /// The caller cancelled the invocation and the guarded process tree's shutdown was requested
    /// and awaited under fixed ceilings.
    ///
    /// A confirmed reap is the normal outcome, not a promise. [`PROCESS_CONTROL_LIMIT`] bounds the
    /// process-control call and [`CHILD_REAP_LIMIT`] bounds collecting the child, and when either
    /// expires this is still reported as `Cancelled`, because the user's interruption is what
    /// happened and there is no other outcome Zuno observed. What can survive an expiry is bounded by
    /// what the operating system honours: `kill_on_drop` still terminates the direct child, so an
    /// unconfirmed reap means a helper the product had already detached from the guarded group, or on
    /// Windows a tree walk that did not finish. This is stated here so a later change does not build
    /// on the stronger claim.
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

    if let Err(error) = send_rpc(
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
    {
        return Err(ProductAgentError::Incompatible {
            product: "Codex",
            message: write_failure(&mut child, &agent.environment, stderr_task, error).await,
        });
    }
    if let Err(error) = wait_for_response(
        &mut reader,
        &mut writer,
        CODEX_INITIALIZE_ID,
        false,
        &cancellation,
    )
    .await
    {
        return Err(
            handshake_failure("Codex", &mut child, &agent.environment, stderr_task, error).await,
        );
    }
    if let Err(error) = send_rpc(&mut writer, json!({"method":"initialized","params":{}})).await {
        return Err(ProductAgentError::Incompatible {
            product: "Codex",
            message: write_failure(&mut child, &agent.environment, stderr_task, error).await,
        });
    }

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
                    return Err(handshake_failure(
                        "Codex",
                        &mut child,
                        &agent.environment,
                        stderr_task,
                        error,
                    )
                    .await);
                }
            }
        }
        Err(error) => {
            return Err(handshake_failure(
                "Codex",
                &mut child,
                &agent.environment,
                stderr_task,
                error,
            )
            .await);
        }
    };
    let thread_id = match thread.pointer("/result/thread/id").and_then(Value::as_str) {
        Some(thread_id) => thread_id.to_owned(),
        // Routed through the shared handshake exit rather than returned with `?`, which reaped
        // nothing and drained nothing: the guarded group was left to `kill_on_drop`, which reaches
        // only the direct child, and the stderr reader was left looping. The classification is
        // unchanged; a response missing the id it is defined to carry is a protocol incompatibility.
        None => {
            return Err(handshake_failure(
                "Codex",
                &mut child,
                &agent.environment,
                stderr_task,
                RpcResponseError::transport(
                    "thread/start response did not contain result.thread.id",
                ),
            )
            .await);
        }
    };

    if let Err(error) = send_rpc(
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
    {
        // Uncertain, not failed: a write that stopped part way may still have delivered a complete
        // line, so the turn may already be editing the user's directory.
        return Err(ProductAgentError::Uncertain {
            product: "Codex",
            message: write_failure(&mut child, &agent.environment, stderr_task, error).await,
        });
    }
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
            return Err(turn_start_failure(
                "Codex",
                &mut child,
                &agent.environment,
                stderr_task,
                error,
            )
            .await);
        }
    };
    let turn_id = match turn.pointer("/result/turn/id").and_then(Value::as_str) {
        Some(turn_id) => turn_id.to_owned(),
        // The same exit as any other failure around an accepted `turn/start`, for the same reason,
        // and uncertain for the reason that exit documents: the request is already flushed.
        None => {
            return Err(turn_start_failure(
                "Codex",
                &mut child,
                &agent.environment,
                stderr_task,
                RpcResponseError::transport("turn/start response did not contain result.turn.id"),
            )
            .await);
        }
    };

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
                let _stderr = finish_stderr(stderr_task).await;
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
                let stderr = finish_stderr(stderr_task).await;
                return Err(ProductAgentError::Uncertain {
                    product: "Codex",
                    message: diagnostic(&agent.environment, error.to_string(), &stderr),
                });
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
                    .and_then(Value::as_str);
                let error = notification_turn
                    .and_then(|turn| turn.get("error"))
                    .filter(|error| !error.is_null())
                    .map(ToString::to_string);
                // One value, read from the whole turn, both decides the verdict and is what the
                // verdict claims: the product reported this turn as failed *and* named the sandbox.
                // A refusal code attached to a turn the product did not report as failed decides
                // nothing, so the arms below cannot reach `Denied` from a completed turn whose
                // answer happened to be blank.
                let denied = codex_failed_turn_was_sandbox_denied(notification_turn);
                terminate(&mut child).await;
                let stderr = finish_stderr(stderr_task).await;
                return match status {
                    Some("completed") if !final_text.trim().is_empty() => {
                        Ok(ProductAgentResult { text: final_text })
                    }
                    Some("interrupted") => Err(ProductAgentError::Cancelled { product: "Codex" }),
                    status => {
                        let failure = error.unwrap_or_else(|| match status {
                            Some(status) => format!("turn ended with status `{status}`"),
                            None => "turn/completed did not state a status".to_owned(),
                        });
                        let message = diagnostic(&agent.environment, failure, &stderr);
                        if denied {
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
                let _stderr = finish_stderr(stderr_task).await;
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
            let status = reap_after_stdout_eof(&mut child).await;
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
        // One value selects the branch and carries what that branch reports, so no arm can be
        // entered for one reason and labelled with another.
        let outcome = claude_turn_outcome(&message);
        terminate(&mut child).await;
        let stderr = finish_stderr(stderr_task).await;
        return match outcome {
            ClaudeTurnOutcome::Answered(text) => Ok(ProductAgentResult { text }),
            ClaudeTurnOutcome::Failed(failure) => Err(ProductAgentError::Failed {
                product: "Claude Code",
                message: diagnostic(&agent.environment, failure, &stderr),
            }),
            ClaudeTurnOutcome::Refused(failure) => Err(ProductAgentError::Denied {
                product: "Claude Code",
                message: diagnostic(&agent.environment, failure, &stderr),
            }),
        };
    }
}

/// What one Claude Code `result` frame says about its own turn.
///
/// Each variant carries the text the outcome is reported with, so the value that selects a branch is
/// the value that describes it. Splitting those two apart is what let a turn the product reported as
/// successful be labelled a permission failure: the denial record was consulted on a branch chosen
/// by whether the answer was blank, and a blank answer is a missing answer, not a refusal.
enum ClaudeTurnOutcome {
    /// The product reported no error and the frame carries an answer.
    Answered(String),
    /// The turn produced nothing a caller can act on, carrying the product's own text where it has
    /// any and naming the frame's `subtype` where it has none.
    Failed(String),
    /// The product reported the turn as failed *and* recorded a refused tool call.
    Refused(String),
}

/// Classify one `result` frame, deciding the outcome and its description together.
///
/// `is_error` is the product's own statement that the turn failed and is the only thing consulted
/// for that: `subtype` deliberately is not, even though its four error values are documented,
/// because a frame that carries a real answer alongside `error_max_turns` is a capped turn whose
/// partial answer the caller can still use, and turning that into a hard failure would discard a
/// usable result on the strength of a string. A blank answer is reported as
/// [`ClaudeTurnOutcome::Failed`] naming the subtype, which is where such a frame lands anyway, and
/// [`ClaudeTurnOutcome::Refused`] is unreachable without `is_error`.
///
/// Blankness is tested once, with `trim`. Testing it one way to decide and another way to describe
/// is how an all-whitespace answer became the entire text of a failure diagnostic.
fn claude_turn_outcome(message: &Value) -> ClaudeTurnOutcome {
    let answer = message
        .get("result")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let errored = message
        .get("is_error")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !errored && !answer.trim().is_empty() {
        return ClaudeTurnOutcome::Answered(answer.to_owned());
    }
    let described = if answer.trim().is_empty() {
        let subtype = message
            .get("subtype")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        format!("result subtype `{subtype}`")
    } else {
        answer.to_owned()
    };
    if claude_code_failed_turn_refused_a_tool(message) {
        ClaudeTurnOutcome::Refused(described)
    } else {
        ClaudeTurnOutcome::Failed(described)
    }
}

/// Whether the product reported this Codex turn as failed *and* named its own sandbox as the cause.
///
/// Both halves are read here, from the same value, so a caller cannot enter one branch and label it
/// with the other. `status` is the product's own statement of the turn's outcome and `error` is
/// documented as populated only when that status is `failed`, so a refusal code arriving on any other
/// status is a frame contradicting itself: it decides nothing, and in particular a `completed` turn
/// whose answer happened to be blank is a turn with no answer, never a permission failure. A turn
/// that states no status at all is not resolvable and fails closed the same way, rather than
/// defaulting to the verdict.
///
/// Read from the app-server's own typed error code, never from text. `turn.error` is a `TurnError`
/// of `{message, additionalDetails, codexErrorInfo}`, and `codexErrorInfo` is an enum whose
/// `sandboxError` variant is the sandbox refusal; the other plain variants are
/// `contextWindowExceeded`, `sessionBudgetExceeded`, `usageLimitExceeded`, `serverOverloaded`,
/// `cyberPolicy`, `misalignmentPolicyViolation`, `internalServerError`, `unauthorized`,
/// `badRequest`, `threadRollbackFailed` and `other`, and the remaining variants are objects, for
/// which `as_str` correctly yields nothing. `unauthorized` is provider-account authentication, not
/// a workspace permission, so it stays a plain failure rather than borrowing the denial label.
///
/// Evidence: `codex app-server generate-json-schema` from the installed Codex 0.150.1, definitions
/// `Turn`, `TurnError` and `CodexErrorInfo`; `Turn.error` is documented there as "Only populated
/// when the Turn's status is failed".
///
/// Text cannot serve. `turn.error.message` is free-form and describes whatever the turn was doing,
/// so a failure to write `permissions/mod.rs` reads as a refusal, and the label a caller acts on
/// would then be chosen by repository contents and by prompt text. An unrecognised or absent code
/// is reported as [`ProductAgentError::Failed`], which still carries the product's own failure text
/// verbatim: the specific claim is dropped, no diagnostic detail is.
///
/// # What a real refusal looks like, and what this can therefore never catch
///
/// Two refusals were driven through the installed Codex 0.150.1 app-server with the exact
/// parameters this adapter sends: `{approvalPolicy: never, sandbox: workspace-write}` writing to
/// `/etc`, and `{approvalPolicy: on-request, sandbox: read-only}` writing inside the thread's own
/// `cwd`. Both ended `turn/completed` with `status: "completed"` and `error: null`; the refusal
/// appeared only as the model's final answer (`zsh:1: read-only file system: /etc/…`), the second
/// case did not even raise an approval request, and both left stderr empty. So a routine native
/// refusal never reaches this function at all: the turn succeeded, and `run` returns the product's
/// own answer, which is the outcome a caller can act on. That was equally true of the substring
/// sniff this replaced, which also only ran on the failed arm, so narrowing to the typed code
/// removed a false-positive path without removing a reachable true positive. It also means there is
/// no stderr signal to fall back to: stderr carried nothing about either refusal.
///
/// What remains is the failed-turn case. `sandboxError` is defined by the installed schema, but no
/// observed run produced it, so [`ProductAgentError::Denied`] may be unreachable in practice
/// against this Codex version. That is deliberate and it fails closed: an unrecognised code is
/// [`ProductAgentError::Failed`] with the product's text, never a claim that the host's permissions
/// must be widened. Nothing outside this crate matches on `Denied`; `zuno-cli`'s job settlement
/// folds it into the same `failed` arm, so the difference is the message a model reads.
fn codex_failed_turn_was_sandbox_denied(turn: Option<&Value>) -> bool {
    let Some(turn) = turn else {
        return false;
    };
    if turn.get("status").and_then(Value::as_str) != Some("failed") {
        return false;
    }
    turn.get("error")
        .filter(|error| !error.is_null())
        .and_then(|error| error.get("codexErrorInfo"))
        .and_then(Value::as_str)
        == Some("sandboxError")
}

/// Whether the product reported this Claude Code turn as failed *and* recorded a refused tool call.
///
/// Both halves are read here, from the same frame, because `permission_denials` records what happened
/// to individual tool calls during the turn and says nothing about the turn's outcome. `is_error` is
/// the outcome. Reading the record without the outcome reports every partially restricted success as
/// a permission failure, and reading it on a branch selected by anything else — the answer being
/// blank, for instance — reports a turn the product called successful as a permission failure with no
/// diagnostic content in it at all.
///
/// Read from `result.permission_denials`, an array of `{tool_name, tool_use_id, tool_input}` minted
/// by the product's permission engine, never from the `result` text, which on `is_error` is
/// model-authored or API-authored prose.
///
/// Evidence: the stream-json schemas embedded in the installed Claude Code 2.1.258. Both `result`
/// variants carry `permission_denials`, and the `system`/`permission_denied` advisory frame's own
/// description names `result.permission_denials` "the authoritative record" while calling the frame
/// itself best-effort. `subtype` cannot serve: its error values are exactly
/// `error_during_execution`, `error_max_turns`, `error_max_budget_usd` and
/// `error_max_structured_output_retries`, none of them a refusal.
///
/// # What a real refusal looks like
///
/// One refusal was driven through the installed Claude Code 2.1.258 with the exact flags this
/// adapter passes (`--print --verbose --output-format stream-json --no-session-persistence
/// --permission-mode dontAsk --disallowedTools AskUserQuestion`), asking it to write `/etc` with
/// Bash. The product refused the tool call, advised it on a `system`/`permission_denied` frame, and
/// ended `{"subtype":"success","is_error":false,"terminal_reason":"completed"}` carrying
/// `permission_denials: [{"tool_name":"Bash","tool_use_id":"toolu_…","tool_input":{"command":"printf
/// hi > /etc/zuno-denial-probe",…}}]`, with the refusal quoted inside `result` and nothing on
/// stderr. So the field really is populated by the real product, and a real refusal usually arrives
/// on a turn that *succeeded*: classifying on the record alone would report every partially
/// restricted success as a permission failure the caller has to recover from, which is why only the
/// failing branch consults this. There is likewise no stderr signal to fall back to.
///
/// [`ProductAgentError::Denied`] is therefore reached when a turn fails while the record is
/// non-empty, for instance a refusal followed by `error_max_turns`. That combination was not
/// observed live, so it is pinned only by fixture; the fallback is [`ProductAgentError::Failed`]
/// with the product's own text, never a claim that permissions must be widened.
fn claude_code_failed_turn_refused_a_tool(message: &Value) -> bool {
    if !message
        .get("is_error")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return false;
    }
    message
        .get("permission_denials")
        .and_then(Value::as_array)
        .is_some_and(|denials| !denials.is_empty())
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

/// Write one JSON-RPC line to the product, refusing to wait on its stdin forever.
///
/// The bound is on the operation rather than on the call sites, because an unbounded write here is
/// a deadlock that no caller can recover from: a product that stops draining stdin while Zuno is
/// mid-write fills the pipe buffer (64 KiB on Linux) and `write_all` never returns, so `run` never
/// settles, no `tokio::select!` on the cancellation token is ever reached, and the tool call hangs
/// for the life of the session. Model-generated prompt text is passed through `turn/start`, so its
/// size is not Zuno's to choose. A healthy product accepts a line in microseconds, so exceeding
/// [`RPC_WRITE_LIMIT`] means the product is wedged.
///
/// The ceiling is on the operation, not on each byte, and the work under it is proportional to an
/// input Zuno does not choose: `ProductAgentRequest.prompt` is model-generated, is not bounded at the
/// tool boundary that fills it in, and is serialised into this one line. A large enough legitimate
/// prompt against a product that parses slowly can therefore expire the ceiling. That is deliberate
/// rather than overlooked: the alternative is refusing an over-long prompt, which belongs at the tool
/// boundary where it can be refused cleanly and not in an adapter that has already spawned the
/// product, and expiry here can only ever produce [`ProductAgentError::Uncertain`] — never a success,
/// and never a clean failure something could replay mechanically. Nothing about the size widens the
/// ceiling itself.
///
/// The timeout is reported as a plain
/// [`std::io::Error`] so every existing caller keeps classifying it exactly as it classifies a
/// broken pipe, which for `turn/start` is [`ProductAgentError::Uncertain`]: a partially flushed
/// line may already have carried a complete request, so the outcome is unknown, never a clean
/// failure that could be replayed.
async fn send_rpc(writer: &mut BufWriter<ChildStdin>, value: Value) -> std::io::Result<()> {
    let mut encoded = serde_json::to_vec(&value).map_err(std::io::Error::other)?;
    encoded.push(b'\n');
    let bytes = encoded.len();
    tokio::time::timeout(RPC_WRITE_LIMIT, async {
        writer.write_all(&encoded).await?;
        writer.flush().await
    })
    .await
    .map_err(|_elapsed| {
        std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!("product did not accept {bytes} bytes of stdin within {RPC_WRITE_LIMIT:?}"),
        )
    })?
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

/// Collect the child's stderr without waiting on a pipe that may never close.
///
/// [`read_bounded`] returns only at EOF, and EOF needs every writer to have closed the pipe. A
/// product that detached a helper into its own process group leaves that helper holding Zuno's
/// stderr after the guarded tree is reaped, so an unbounded wait here would strand the invocation
/// and the cancellation the caller asked for would never settle.
async fn finish_stderr(task: tokio::task::JoinHandle<String>) -> String {
    let mut task = task;
    match tokio::time::timeout(STDERR_DRAIN_LIMIT, &mut task).await {
        Ok(joined) => joined.unwrap_or_default(),
        // Dropping the handle would detach the reader, which loops to EOF even after the limit is
        // reached, so it would keep the pipe read end and its buffer alive for as long as the
        // escaped helper lives. Aborting releases both. Nothing is lost that was not already lost:
        // the timeout path never had a string to report.
        Err(_elapsed) => {
            task.abort();
            String::new()
        }
    }
}

async fn terminate(child: &mut Child) {
    terminate_with(child, zuno_process::request_contained_process_shutdown).await;
}

/// Collect a product whose stdout reported EOF, reaping its tree if it did not leave on its own.
///
/// Stdout closing normally means the product has already exited, so the wait usually returns at
/// once. A product that closes stdout while it keeps running is what the ceiling is for: an
/// unbounded `wait` there holds `run` for as long as that process lives, which for a product that
/// leaked a long-running helper is the rest of the session, and the tool call that owns the
/// invocation never settles. Exceeding [`CHILD_REAP_LIMIT`] is therefore read as "did not leave"
/// and the guarded tree is reaped exactly as a cancellation reaps it.
///
/// The ceiling cannot widen an outcome: `None` is returned rather than a guessed status, and this
/// path reports [`ProductAgentError::Uncertain`] either way, so no caller is told a turn failed
/// cleanly when Zuno never saw how it ended.
async fn reap_after_stdout_eof(child: &mut Child) -> Option<std::process::ExitStatus> {
    let status = tokio::time::timeout(CHILD_REAP_LIMIT, child.wait())
        .await
        .ok()
        .and_then(Result::ok);
    if status.is_none() {
        terminate(child).await;
    }
    status
}

/// Reap one guarded product tree through an injected process-control call.
///
/// `shutdown` is a parameter only so a test can drive this exact function with a call that really
/// does block: on a Unix host the production call is a bare `kill(2)` that returns immediately, so
/// nothing here could otherwise show that the dispatch is off the runtime worker, and a test that
/// called the dispatch helper directly would keep passing after this function was changed back to
/// an inline call.
async fn terminate_with<F>(child: &mut Child, shutdown: F)
where
    F: FnOnce(u32) -> std::io::Result<()> + Send + 'static,
{
    if child.try_wait().ok().flatten().is_none()
        && let Some(pid) = child.id()
    {
        let _ignored = off_runtime_worker(move || shutdown(pid)).await;
    }
    let _ignored = tokio::time::timeout(CHILD_REAP_LIMIT, child.wait()).await;
}

/// Run one blocking process-control call without stalling the runtime worker, and without waiting
/// on it forever.
///
/// `request_contained_process_shutdown` keeps a blocking signature because synchronous callers need
/// it, and on Unix it is a bare `kill(2)`. On Windows the same call spawns `taskkill /f /t` and
/// waits for the whole tree walk. Every session runtime Zuno builds is current-thread, so an inline
/// call would freeze the provider stream and the client event pump until the walk finished, and a
/// cancellation is exactly when those must keep draining.
///
/// The join is bounded because moving the call off the worker does not make it finish: a wedged
/// `taskkill` is the Windows case this dispatch exists for, and an unbounded join would hold
/// `terminate` before its own [`CHILD_REAP_LIMIT`] wait was ever reached. Exceeding the ceiling
/// cannot leave a process more alive than that wait already tolerates, because the reaped child is
/// spawned with `kill_on_drop`, so dropping it still terminates the direct child. Aborting the
/// handle is not attempted: a blocking task that has started cannot be cancelled, so the ceiling
/// releases this task's await, not the operating-system call.
///
/// That has a cost worth stating in the direction it accumulates. On Windows a `taskkill /f /t` that
/// never returns keeps its blocking-pool thread for the life of the process, so repeated
/// cancellations on such a host consume threads from the runtime's blocking pool — tokio's default of
/// 512 per runtime, which no runtime in this workspace overrides, shared with every other blocking
/// user on it — and a pathological host degrades from "this reap was not confirmed" to starving
/// unrelated blocking work. It is strictly better than the inline call it replaced, which froze the
/// whole current-thread runtime on the first occurrence, and it needs a host where that call wedges
/// at all, which no Unix host does: there the call is a bare `kill(2)`.
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

fn diagnostic(environment: &ChildEnvironment, message: String, stderr: &str) -> String {
    let message = environment.safe(message);
    let stderr = environment.safe(stderr.trim());
    if stderr.is_empty() {
        message
    } else {
        format!("{message}; stderr: {stderr}")
    }
}

/// Reap the tree, drain stderr, and render one failure to write to the product's stdin.
///
/// Shared by every write exit so none of them can skip the reap. Before [`send_rpc`] had a ceiling
/// these paths needed a broken pipe to be reached; a wedged product reaches them routinely now, and
/// returning early without reaping would leave whatever the product had already spawned running and
/// would detach the stderr reader, which loops to EOF.
async fn write_failure(
    child: &mut Child,
    environment: &ChildEnvironment,
    stderr_task: tokio::task::JoinHandle<String>,
    error: std::io::Error,
) -> String {
    terminate(child).await;
    let stderr = finish_stderr(stderr_task).await;
    diagnostic(environment, error.to_string(), &stderr)
}

/// Reap the tree, settle the stderr reader, and classify a failure raised before `turn/start` was
/// written.
///
/// The reader is taken by value for the same reason [`write_failure`] takes it: it returns only at
/// EOF, and a product that detached a helper into its own process group leaves that helper holding
/// Zuno's stderr pipe after the guarded tree is reaped. An exit that dropped the handle instead of
/// passing it to [`finish_stderr`] would leave the reader looping for the life of that helper,
/// holding the pipe read end and its buffer, once per invocation. Passing it also means the product's
/// own stderr reaches the reported diagnostic, which for a handshake failure is the only place an
/// installed product that cannot speak the protocol explains itself.
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
    stderr_task: tokio::task::JoinHandle<String>,
    error: RpcResponseError,
) -> ProductAgentError {
    terminate(child).await;
    let stderr = finish_stderr(stderr_task).await;
    if error.cancelled {
        ProductAgentError::Cancelled { product }
    } else {
        ProductAgentError::Incompatible {
            product,
            message: diagnostic(environment, error.to_string(), &stderr),
        }
    }
}

/// Reap the tree, settle the stderr reader, and classify a failure raised while the `turn/start`
/// response is outstanding.
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
    stderr_task: tokio::task::JoinHandle<String>,
    error: RpcResponseError,
) -> ProductAgentError {
    terminate(child).await;
    let stderr = finish_stderr(stderr_task).await;
    let failure = if error.cancelled {
        "invocation was cancelled while the turn/start response was outstanding; the turn may \
         already have started and changed the working directory"
            .to_owned()
    } else {
        error.to_string()
    };
    ProductAgentError::Uncertain {
        product,
        message: diagnostic(environment, failure, &stderr),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;
    use tokio::process::Child;

    /// Long enough that a starved current-thread runtime is unmistakable, short enough to keep this
    /// test sub-second.
    const BLOCKING_CALL: Duration = Duration::from_millis(300);
    const TICK: Duration = Duration::from_millis(5);
    /// Longer than [`super::PROCESS_CONTROL_LIMIT`], so the ceiling is what ends the wait.
    const WEDGED_CALL: Duration = Duration::from_secs(20);
    /// How long a stand-in product stays alive: longer than every ceiling under test plus the slack
    /// each test allows, so a test can only pass because a ceiling fired, never because the child
    /// happened to exit.
    const LIVE_CHILD_SECONDS: u32 = 30;
    /// Slack over a ceiling, for process spawn and scheduling on a loaded machine.
    const SLACK: Duration = Duration::from_secs(5);

    /// The shape every Zuno session runtime has, which is what makes an inline blocking call fatal.
    fn current_thread_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("current-thread runtime")
    }

    /// One live child that outlives the measurement, on every supported platform.
    ///
    /// `sleep` and `ping` are invoked directly rather than through a shell, so no POSIX quoting or
    /// `sh` availability is assumed on either family.
    fn sleeping_child(seconds: u32) -> Child {
        sleeping_child_with_stdin(seconds, std::process::Stdio::null())
    }

    /// The same child with a caller-chosen stdin, for the write-side ceiling.
    ///
    /// Neither `sleep` nor `ping` ever reads stdin, so a piped stdin is a pipe with a live reader
    /// end that is never drained: exactly the product this adapter has to survive.
    fn sleeping_child_with_stdin(seconds: u32, stdin: std::process::Stdio) -> Child {
        let mut command =
            tokio::process::Command::new(if cfg!(windows) { "ping" } else { "sleep" });
        if cfg!(windows) {
            command.args(["-n", &(seconds + 1).to_string(), "127.0.0.1"]);
        } else {
            command.arg(seconds.to_string());
        }
        command
            .stdin(stdin)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("sleeping child")
    }

    /// A task only a free runtime worker can poll, so its counter measures worker availability.
    fn spawn_ticker(ticks: &Arc<AtomicU64>) -> tokio::task::JoinHandle<()> {
        let ticks = Arc::clone(ticks);
        tokio::spawn(async move {
            loop {
                ticks.fetch_add(1, Ordering::Release);
                tokio::time::sleep(TICK).await;
            }
        })
    }

    /// How many ticks the runtime managed while a 300 ms process-control call was in flight.
    ///
    /// The count is taken from inside the call itself, not around [`super::terminate_with`], because
    /// the bounded `child.wait()` that follows yields either way: only a reading taken while the
    /// blocking work is running can tell an inline call from a dispatched one.
    fn ticks_during_terminate() -> u64 {
        let runtime = current_thread_runtime();
        let observed = runtime.block_on(async {
            let ticks = Arc::new(AtomicU64::new(0));
            let observed = Arc::new(AtomicU64::new(0));
            let ticker = spawn_ticker(&ticks);
            tokio::task::yield_now().await;
            let mut child = sleeping_child(LIVE_CHILD_SECONDS);
            let shutdown = {
                let ticks = Arc::clone(&ticks);
                let observed = Arc::clone(&observed);
                move |pid: u32| {
                    let before = ticks.load(Ordering::Acquire);
                    std::thread::sleep(BLOCKING_CALL);
                    observed.store(
                        ticks.load(Ordering::Acquire).saturating_sub(before),
                        Ordering::Release,
                    );
                    zuno_process::request_contained_process_shutdown(pid)
                }
            };
            super::terminate_with(&mut child, shutdown).await;
            ticker.abort();
            observed.load(Ordering::Acquire)
        });
        runtime.shutdown_background();
        observed
    }

    /// The same measurement with the same call left inline, as a control.
    ///
    /// This is not coverage of anything Zuno owns; it exists so a non-zero reading above cannot be
    /// mistaken for a measurement that reports progress unconditionally.
    fn ticks_during_an_inline_call() -> u64 {
        let runtime = current_thread_runtime();
        let observed = runtime.block_on(async {
            let ticks = Arc::new(AtomicU64::new(0));
            let ticker = spawn_ticker(&ticks);
            tokio::task::yield_now().await;
            let before = ticks.load(Ordering::Acquire);
            std::thread::sleep(BLOCKING_CALL);
            let observed = ticks.load(Ordering::Acquire).saturating_sub(before);
            ticker.abort();
            observed
        });
        runtime.shutdown_background();
        observed
    }

    /// Reaping a product tree must not stop the rest of the runtime from running.
    ///
    /// `terminate` reaches `taskkill /pid N /f /t` on Windows, where the call spawns a process and
    /// waits for a tree walk. Every session runtime is current-thread, so calling that inline would
    /// leave the provider stream reader, the other in-flight tool calls and the client event pump
    /// unpollable for the duration of a cancellation. The measurement is taken inside
    /// `terminate_with`'s own process-control call, so restoring the inline call in that function
    /// makes this fail.
    #[test]
    fn terminate_dispatches_process_control_off_the_runtime_worker() {
        assert_eq!(
            ticks_during_an_inline_call(),
            0,
            "control: an inline blocking call must starve the current-thread runtime, otherwise \
             this measurement cannot detect one"
        );
        assert!(
            ticks_during_terminate() > 1,
            "other tasks must keep being polled while terminate's process-control call blocks"
        );
    }

    /// Moving the call off the worker does not make it finish, so the join must be bounded.
    ///
    /// A wedged `taskkill /f /t` is the exact Windows case the dispatch exists for. Without the
    /// ceiling, `terminate` never reaches its own `child.wait()` bound, so the cancellation that
    /// asked for the reap never settles and the product-agent tool call hangs for the session.
    #[test]
    fn terminate_settles_when_the_process_control_call_wedges() {
        let runtime = current_thread_runtime();
        let settled = runtime.block_on(async {
            let mut child = sleeping_child(LIVE_CHILD_SECONDS);
            let wedged = |_pid: u32| {
                std::thread::sleep(WEDGED_CALL);
                Ok(())
            };
            tokio::time::timeout(
                super::PROCESS_CONTROL_LIMIT + super::CHILD_REAP_LIMIT + SLACK,
                super::terminate_with(&mut child, wedged),
            )
            .await
            .is_ok()
        });
        runtime.shutdown_background();
        assert!(
            settled,
            "terminate must settle even when the process-control call never returns"
        );
    }

    /// A product that closed stdout while still running must be reaped, not waited on.
    ///
    /// This is the stdout-EOF exit of the Claude Code loop: `read == 0` while the child is alive.
    /// The stand-in product outlives every ceiling under test, so an unbounded `child.wait()` there
    /// holds `run` for the whole life of that process, and for a product that leaked a helper that
    /// is the rest of the session. Settling is not enough on its own: the tree must actually be
    /// gone, otherwise the ceiling would trade a hang for an abandoned process.
    #[test]
    fn a_product_that_closed_stdout_but_kept_running_is_reaped() {
        let runtime = current_thread_runtime();
        let (settled, reaped, elapsed) = runtime.block_on(async {
            let mut child = sleeping_child(LIVE_CHILD_SECONDS);
            let started = std::time::Instant::now();
            let settled = tokio::time::timeout(
                super::CHILD_REAP_LIMIT
                    + super::PROCESS_CONTROL_LIMIT
                    + super::CHILD_REAP_LIMIT
                    + SLACK,
                super::reap_after_stdout_eof(&mut child),
            )
            .await;
            let elapsed = started.elapsed();
            let reaped = child.try_wait().ok().flatten().is_some();
            (settled, reaped, elapsed)
        });
        runtime.shutdown_background();
        let status = settled.expect("the reap must settle instead of waiting out the live product");
        assert!(
            status.is_none(),
            "a product that never left cannot have reported an exit status: {status:?}"
        );
        assert!(
            elapsed >= super::CHILD_REAP_LIMIT,
            "the ceiling must be what ended the wait, not an early exit: {elapsed:?}"
        );
        assert!(
            reaped,
            "the guarded tree must be reaped when the product does not leave on its own"
        );
    }

    /// Writing one JSON-RPC line must not wait forever on a product that stopped reading stdin.
    ///
    /// The prompt is model-generated, so its size is not Zuno's to choose. Once the pipe buffer is
    /// full, 64 KiB on Linux, an unbounded `write_all` never returns: `run` stops before any
    /// `tokio::select!` on the cancellation token, so neither the caller's cancel nor any ceiling
    /// further down the settlement path is ever reached. The stand-in product holds the read end
    /// open for far longer than this test waits and never drains it, so only the write's own ceiling
    /// can end this.
    #[test]
    fn writing_a_line_to_a_product_that_never_reads_stdin_settles() {
        let runtime = current_thread_runtime();
        let (settled, elapsed) = runtime.block_on(async {
            let mut child =
                sleeping_child_with_stdin(LIVE_CHILD_SECONDS, std::process::Stdio::piped());
            let stdin = child.stdin.take().expect("piped stdin");
            let mut writer = super::BufWriter::new(stdin);
            let line = serde_json::json!({
                "id": super::CODEX_TURN_START_ID,
                "method": "turn/start",
                "params": {"input":[{"type":"text","text":"x".repeat(512 * 1024)}]}
            });
            let started = std::time::Instant::now();
            let settled = tokio::time::timeout(
                super::RPC_WRITE_LIMIT + SLACK,
                super::send_rpc(&mut writer, line),
            )
            .await;
            (settled, started.elapsed())
        });
        runtime.shutdown_background();
        let error = settled
            .expect("the write must settle instead of waiting on the product")
            .expect_err("a product that never reads its stdin cannot accept 512 KiB");
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut, "{error}");
        assert!(
            elapsed >= super::RPC_WRITE_LIMIT,
            "the ceiling must be what ended the write: {elapsed:?}"
        );
    }
}
