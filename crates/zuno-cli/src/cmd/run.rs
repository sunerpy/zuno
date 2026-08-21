//! The headless turn surface: one prompt in, the turn's events printed.
//!
//! Everything about *composing* a turn moved to [`super::turn`] when the TUI gained
//! the ability to drive one, so what is left here is this surface's own two jobs —
//! deciding which invocations it can honour, and rendering
//! [`zuno_engine::r#loop::TurnEvent`]s as text. The composition is shared precisely so
//! that a tool, a permission rule, or a session-resolution fix cannot land on one
//! surface and miss the other.

use std::io::{IsTerminal as _, Read as _, Write};
use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{Value, json};
use zuno_engine::r#loop::{TurnEvent, event_channel};
use zuno_llm::event::{ConnectionPhase, StreamEvent};

use crate::cmd::turn::{SessionChoice, TurnHost, TurnOptions, TurnPlan};
use crate::command::{RunArgs, RunFormat};
use crate::environment::StartupEnvironment;

pub(super) fn execute(args: &RunArgs, environment: &StartupEnvironment) -> Result<(), String> {
    validate_flags(args)?;
    let message = if args.command.is_some() {
        args.message.join(" ")
    } else {
        prompt(args)?
    };
    let options = TurnOptions {
        directory: args.dir.as_deref().map(PathBuf::from),
        model: args.model.clone(),
        agent: args.agent.clone(),
        session: SessionChoice::resolve(args.session.as_deref(), args.r#continue),
        title: args.title.clone(),
        effort: None,
    };
    let runtime = runtime()?;
    let plan = runtime.block_on(TurnPlan::resolve(&options, environment))?;
    // Before the host, not after: the registry reads the MCP loader once while it is
    // being assembled, and this surface has no second turn for a late connection to
    // appear in. See `super::mcp_runtime`.
    let mcp = super::mcp_runtime::McpRuntime::from_config(
        plan.config(),
        plan.worktree().unwrap_or_else(|| plan.directory()),
    );
    let mcp_notes = match mcp.as_ref() {
        Some(mcp) => runtime.block_on(mcp.connect()),
        None => Vec::new(),
    };
    let mut host = TurnHost::open_with_mcp(
        plan,
        environment,
        Arc::new(crate::cmd::tool_runtime::HeadlessApproval),
        mcp.as_ref().map(super::mcp_runtime::McpRuntime::catalog),
    )?;
    host.push_notes(mcp_notes);

    let (sender, receiver) = event_channel();
    let sender = host.with_event_hooks(sender);
    let (outcome, rendered) = runtime.block_on(async {
        tokio::join!(
            async {
                match args.command.as_deref() {
                    Some(command) => {
                        host.drive_command(command, &message, sender.clone())
                            .await?
                    }
                    None => host.drive(&message, sender.clone()).await?,
                }
                while host
                    .continue_goal_if_idle(zuno_goal::QueuedUserInput::Absent, sender.clone())
                    .await?
                {}
                drop(sender);
                Ok::<(), String>(())
            },
            render_events(receiver, args.format)
        )
    });
    runtime.block_on(host.shutdown());
    if let Some(mcp) = mcp {
        runtime.block_on(mcp.shutdown());
    }
    outcome?;
    rendered?;
    Ok(())
}

fn validate_flags(args: &RunArgs) -> Result<(), String> {
    if args.fork {
        return Err(
            "--fork requires a session-history fork API that is not available yet".to_owned(),
        );
    }
    if args.share {
        return Err("--share is not available in the local Rust runtime".to_owned());
    }
    if !args.file.is_empty() {
        return Err("--file attachment projection is not available yet".to_owned());
    }
    if args.attach.is_some()
        || args.port.is_some()
        || args.username.is_some()
        || args.password.is_some()
    {
        return Err("--attach/--port/--username/--password require the remote SDK client, which is not available yet".to_owned());
    }
    if args.variant.is_some() || args.thinking {
        return Err(
            "--variant and --thinking are not available in the current provider request facade"
                .to_owned(),
        );
    }
    // Both are interactive-surface flags, and the interactive surface now honours
    // them: `--auto` is `tui --auto`. Refusing here rather than quietly ignoring them
    // keeps a scripted caller from believing a headless run auto-approved anything.
    if args.interactive || args.auto {
        return Err(
            "--interactive and --auto belong to the interactive surface; run `tui --auto --prompt <message>` instead"
                .to_owned(),
        );
    }
    if args.r#continue && args.session.is_some() {
        return Err("--continue and --session cannot be used together".to_owned());
    }
    Ok(())
}

fn prompt(args: &RunArgs) -> Result<String, String> {
    if !args.message.is_empty() {
        return Ok(args.message.join(" "));
    }
    if std::io::stdin().is_terminal() {
        return Err("a message is required (or pipe one on stdin)".to_owned());
    }
    let mut message = String::new();
    std::io::stdin()
        .read_to_string(&mut message)
        .map_err(to_string)?;
    let message = message.trim().to_owned();
    if message.is_empty() {
        return Err("a message is required".to_owned());
    }
    Ok(message)
}

async fn render_events(
    receiver: tokio::sync::mpsc::Receiver<TurnEvent>,
    format: RunFormat,
) -> Result<(), String> {
    let stderr_is_terminal = std::io::stderr().is_terminal();
    let mut stdout = std::io::stdout().lock();
    let mut stderr = std::io::stderr().lock();
    render_events_to(
        receiver,
        format,
        &mut stdout,
        &mut stderr,
        stderr_is_terminal,
    )
    .await
}

async fn render_events_to<Stdout, Stderr>(
    mut receiver: tokio::sync::mpsc::Receiver<TurnEvent>,
    format: RunFormat,
    stdout: &mut Stdout,
    stderr: &mut Stderr,
    stderr_is_terminal: bool,
) -> Result<(), String>
where
    Stdout: Write,
    Stderr: Write,
{
    let mut wrote_text = false;
    while let Some(event) = receiver.recv().await {
        match format {
            RunFormat::Default => match event {
                TurnEvent::Provider {
                    event: StreamEvent::TextDelta(text),
                    ..
                } => {
                    write!(stdout, "{text}").map_err(to_string)?;
                    stdout.flush().map_err(to_string)?;
                    wrote_text = true;
                }
                TurnEvent::Provider {
                    event: StreamEvent::Error { message, .. },
                    ..
                } => writeln!(stderr, "{message}").map_err(to_string)?,
                TurnEvent::Provider {
                    event: StreamEvent::RetryRollback { attempt, max },
                    ..
                } => write_retry_notice(stderr, attempt, max, stderr_is_terminal)
                    .map_err(to_string)?,
                // Status details were only ever rendered by `--json`, so anything the
                // prelude reported — a suppressed tool, a skipped internal — was
                // invisible to a plain run. stderr, because stdout is the model's
                // answer and a caller pipes it.
                TurnEvent::Provider {
                    event: StreamEvent::StatusDetail { detail },
                    ..
                } => writeln!(stderr, "{detail}").map_err(to_string)?,
                TurnEvent::ToolDispatchStarted { name, .. } => {
                    writeln!(stderr, "[{name}] started").map_err(to_string)?;
                }
                TurnEvent::ToolDispatchCompleted {
                    name,
                    title,
                    is_error,
                    ..
                } => writeln!(
                    stderr,
                    "[{name}] {}: {title}",
                    if is_error { "failed" } else { "completed" }
                )
                .map_err(to_string)?,
                _ => {}
            },
            RunFormat::Json => writeln!(stdout, "{}", event_json(event)).map_err(to_string)?,
        }
    }
    if format == RunFormat::Default && wrote_text {
        writeln!(stdout).map_err(to_string)?;
    }
    Ok(())
}

fn write_retry_notice(
    writer: &mut impl Write,
    attempt: u32,
    max: u32,
    use_color: bool,
) -> std::io::Result<()> {
    let notice = format!("Retrying provider request (attempt {attempt}/{max})");
    if use_color {
        writeln!(writer, "\x1b[31m{notice}\x1b[0m")
    } else {
        writeln!(writer, "{notice}")
    }
}

fn event_json(event: TurnEvent) -> Value {
    match event {
        TurnEvent::TurnStarted { session_id } => {
            json!({"type":"turn_started","sessionID":session_id})
        }
        TurnEvent::HistoryRepaired {
            repaired_tool_results,
        } => json!({"type":"history_repaired","repairedToolResults":repaired_tool_results}),
        TurnEvent::AgentResolved { step, agent } => {
            json!({"type":"agent_resolved","step":step,"agent":agent})
        }
        TurnEvent::ModelResolved {
            step,
            provider_id,
            model_id,
        } => {
            json!({"type":"model_resolved","step":step,"providerID":provider_id,"modelID":model_id})
        }
        TurnEvent::AssistantMessageCreated { step, message_id } => {
            json!({"type":"assistant_message_created","step":step,"messageID":message_id})
        }
        TurnEvent::ToolSnapshotLocked {
            step,
            tool_ids,
            rebuilt_for_late_mcp,
        } => {
            json!({"type":"tool_snapshot_locked","step":step,"toolIDs":tool_ids,"rebuiltForLateMcp":rebuilt_for_late_mcp})
        }
        TurnEvent::ProviderRequestStarted {
            step,
            message_count,
        } => json!({"type":"provider_request_started","step":step,"messageCount":message_count}),
        TurnEvent::Provider { step, event } => stream_event_json(step, event),
        TurnEvent::AssistantCheckpointed {
            step,
            message_id,
            interrupted,
        } => {
            json!({"type":"assistant_checkpointed","step":step,"messageID":message_id,"interrupted":interrupted})
        }
        TurnEvent::ToolDispatchStarted {
            step,
            call_id,
            name,
        } => json!({"type":"tool_dispatch_started","step":step,"callID":call_id,"name":name}),
        TurnEvent::ToolDispatchCompleted {
            step,
            call_id,
            name,
            title,
            output,
            diff,
            written_paths,
            is_error,
        } => {
            json!({"type":"tool_dispatch_completed","step":step,"callID":call_id,"name":name,"title":title,"output":output,"diff":diff,"writtenPaths":written_paths,"isError":is_error})
        }
        TurnEvent::ToolResultAppended {
            step,
            call_id,
            is_error,
        } => json!({"type":"tool_result_appended","step":step,"callID":call_id,"isError":is_error}),
        TurnEvent::StepCompleted {
            step,
            finish_reason,
        } => {
            json!({"type":"step_completed","step":step,"finishReason":finish_reason.map(|reason| format!("{reason:?}"))})
        }
        TurnEvent::TurnCompleted {
            assistant_message_id,
            steps,
        } => json!({"type":"turn_completed","messageID":assistant_message_id,"steps":steps}),
        TurnEvent::TurnInterrupted {
            assistant_message_id,
            steps,
        } => json!({"type":"turn_interrupted","messageID":assistant_message_id,"steps":steps}),
    }
}

fn stream_event_json(step: u32, event: StreamEvent) -> Value {
    match event {
        StreamEvent::TextDelta(text) => json!({"type":"text","step":step,"text":text}),
        StreamEvent::ToolUseStart { id, name } => {
            json!({"type":"tool_use_start","step":step,"id":id,"name":name})
        }
        StreamEvent::ToolInputDelta(delta) => {
            json!({"type":"tool_input_delta","step":step,"delta":delta})
        }
        StreamEvent::ToolUseEnd => json!({"type":"tool_use_end","step":step}),
        StreamEvent::ToolUseSignature(signature) => {
            json!({"type":"tool_use_signature","step":step,"signature":format!("{signature:?}")})
        }
        StreamEvent::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => {
            json!({"type":"tool_result","step":step,"toolUseID":tool_use_id,"content":content,"isError":is_error})
        }
        StreamEvent::GeneratedImage {
            id,
            path,
            metadata_path,
            output_format,
            revised_prompt,
        } => {
            json!({"type":"generated_image","step":step,"id":id,"path":path,"metadataPath":metadata_path,"outputFormat":output_format,"revisedPrompt":revised_prompt})
        }
        StreamEvent::ReasoningStart => json!({"type":"reasoning_start","step":step}),
        StreamEvent::ReasoningDelta(text) => json!({"type":"reasoning","step":step,"text":text}),
        StreamEvent::ReasoningSignatureDelta(delta) => {
            json!({"type":"reasoning_signature_delta","step":step,"delta":delta})
        }
        StreamEvent::ProviderReasoningItem {
            id,
            summary,
            encrypted_content,
            status,
        } => {
            json!({"type":"provider_reasoning_item","step":step,"id":id,"summary":summary,"encryptedContent":encrypted_content,"status":status})
        }
        StreamEvent::ReasoningEnd => json!({"type":"reasoning_end","step":step}),
        StreamEvent::ReasoningDone { duration_secs } => {
            json!({"type":"reasoning_done","step":step,"durationSecs":duration_secs})
        }
        StreamEvent::MessageEnd { stop_reason } => {
            json!({"type":"message_end","step":step,"stopReason":stop_reason.map(|reason| format!("{reason:?}"))})
        }
        StreamEvent::RetryRollback { attempt, max } => {
            json!({"type":"retry_rollback","step":step,"attempt":attempt,"max":max})
        }
        StreamEvent::TokenUsage {
            input_tokens,
            output_tokens,
            cache_read_input_tokens,
            cache_write_input_tokens,
            accounting,
        } => {
            json!({"type":"token_usage","step":step,"inputTokens":input_tokens,"outputTokens":output_tokens,"cacheReadInputTokens":cache_read_input_tokens,"cacheWriteInputTokens":cache_write_input_tokens,"promptAccounting":accounting.as_str()})
        }
        StreamEvent::ConnectionType { connection } => {
            json!({"type":"connection_type","step":step,"connection":connection})
        }
        StreamEvent::ConnectionPhase { phase } => {
            json!({"type":"connection_phase","step":step,"phase":connection_phase(phase)})
        }
        StreamEvent::StatusDetail { detail } => {
            json!({"type":"status_detail","step":step,"detail":detail})
        }
        StreamEvent::Error {
            message,
            retry_after,
        } => {
            json!({"type":"error","step":step,"message":message,"retryAfterMs":retry_after.map(|duration| duration.as_millis())})
        }
        StreamEvent::SessionId(session_id) => {
            json!({"type":"session_id","step":step,"sessionID":session_id})
        }
        StreamEvent::Compaction {
            trigger,
            pre_tokens,
            openai_encrypted_content,
        } => {
            json!({"type":"compaction","step":step,"trigger":trigger,"preTokens":pre_tokens,"openaiEncryptedContent":openai_encrypted_content})
        }
        StreamEvent::UpstreamProvider { provider } => {
            json!({"type":"upstream_provider","step":step,"provider":provider})
        }
        StreamEvent::NativeToolCall {
            request_id,
            tool_name,
            input,
        } => {
            json!({"type":"native_tool_call","step":step,"requestID":request_id,"toolName":tool_name,"input":input})
        }
    }
}

fn connection_phase(phase: ConnectionPhase) -> Value {
    match phase {
        ConnectionPhase::Authenticating => json!("authenticating"),
        ConnectionPhase::Connecting => json!("connecting"),
        ConnectionPhase::SendingRequest => json!("sending_request"),
        ConnectionPhase::WaitingForResponse => json!("waiting_for_response"),
        ConnectionPhase::Streaming => json!("streaming"),
        ConnectionPhase::Retrying { attempt, max } => {
            json!({"type":"retrying","attempt":attempt,"max":max})
        }
    }
}

fn runtime() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(to_string)
}

fn to_string(error: impl std::fmt::Display) -> String {
    error.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_args() -> RunArgs {
        RunArgs {
            message: vec!["hello".to_owned()],
            command: None,
            r#continue: false,
            session: None,
            fork: false,
            share: false,
            model: None,
            agent: None,
            format: RunFormat::Default,
            file: Vec::new(),
            title: None,
            attach: None,
            password: None,
            username: None,
            dir: None,
            port: None,
            variant: None,
            thinking: false,
            interactive: false,
            auto: false,
        }
    }

    #[test]
    fn unsupported_remote_and_session_flags_are_rejected_before_side_effects() {
        let mut args = run_args();
        args.r#continue = true;
        args.session = Some("ses_x".to_owned());
        assert!(validate_flags(&args).is_err());
    }

    #[tokio::test]
    async fn renderer_drains_a_bounded_event_channel_without_deadlock() {
        let (sender, receiver) = tokio::sync::mpsc::channel(1);
        let producer = tokio::spawn(async move {
            for index in 0..70 {
                sender
                    .send(TurnEvent::TurnStarted {
                        session_id: format!("ses_{index}"),
                    })
                    .await
                    .expect("renderer remains connected");
            }
        });

        tokio::time::timeout(std::time::Duration::from_secs(1), async move {
            let (produced, rendered) =
                tokio::join!(producer, render_events(receiver, RunFormat::Default));
            produced.expect("producer task");
            rendered.expect("render events");
        })
        .await
        .expect("bounded channel must be consumed concurrently");
    }

    async fn rendered_retry_notice(stderr_is_terminal: bool) -> (Vec<u8>, Vec<u8>) {
        let (sender, receiver) = tokio::sync::mpsc::channel(1);
        sender
            .send(TurnEvent::Provider {
                step: 1,
                event: StreamEvent::RetryRollback { attempt: 2, max: 3 },
            })
            .await
            .expect("renderer remains connected");
        drop(sender);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        render_events_to(
            receiver,
            RunFormat::Default,
            &mut stdout,
            &mut stderr,
            stderr_is_terminal,
        )
        .await
        .expect("retry notice renders");
        (stdout, stderr)
    }

    #[tokio::test]
    async fn run_retry_rollback_notice_is_red_on_tty_and_plain_off_tty() {
        let (tty_stdout, tty_stderr) = rendered_retry_notice(true).await;
        assert!(tty_stdout.is_empty());
        assert_eq!(
            tty_stderr,
            b"\x1b[31mRetrying provider request (attempt 2/3)\x1b[0m\n"
        );

        let (pipe_stdout, pipe_stderr) = rendered_retry_notice(false).await;
        assert!(pipe_stdout.is_empty());
        assert_eq!(pipe_stderr, b"Retrying provider request (attempt 2/3)\n");
        assert!(
            !pipe_stderr.contains(&0x1b),
            "non-TTY retry output contains an escape sequence: {pipe_stderr:?}"
        );
    }

    #[test]
    fn run_retry_rollback_json_contract_is_unchanged() {
        assert_eq!(
            stream_event_json(7, StreamEvent::RetryRollback { attempt: 2, max: 3 }),
            json!({"type":"retry_rollback","step":7,"attempt":2,"max":3})
        );
    }
}
