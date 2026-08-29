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
use zuno_llm::event::{ConnectionPhase, RequestContentBlock, StreamEvent};

use crate::cmd::turn::{SessionChoice, TurnHost, TurnOptions, TurnPlan, persisted_session_agent};
use crate::command::{RunArgs, RunFormat};
use crate::environment::StartupEnvironment;

type ProgressPulse<'a> = Option<&'a dyn Fn()>;

const TEXT_ATTACHMENT_MAX_BYTES: usize = 50 * 1_024;
const TEXT_ATTACHMENT_MAX_LINES: usize = 2_000;

pub(super) fn execute(
    args: &RunArgs,
    environment: &StartupEnvironment,
    progress: ProgressPulse<'_>,
) -> Result<(), String> {
    validate_flags(args)?;
    let message = if args.command.is_some() {
        args.message.join(" ")
    } else {
        prompt(args)?
    };
    let file_content = file_attachment_content(&message, &args.file)?;
    let session = SessionChoice::resolve(args.session.as_deref(), args.r#continue);
    let options = TurnOptions {
        directory: args.dir.as_deref().map(PathBuf::from),
        model: args.model.clone(),
        agent: args
            .agent
            .clone()
            .or_else(|| persisted_session_agent(&session)),
        preset: None,
        session,
        title: args.title.clone(),
        effort: None,
        variant: args.variant.clone(),
        thinking: args.thinking,
        tool_authority: None,
        extension_composition: super::turn::ExtensionComposition::Active,
    };
    report_progress(progress);
    let runtime = runtime()?;
    let plan = runtime.block_on(TurnPlan::resolve(&options, environment))?;
    report_progress(progress);
    // Before the host, not after: the registry reads the MCP loader once while it is
    // being assembled, and this surface has no second turn for a late connection to
    // appear in. See `super::mcp_runtime`.
    let mcp = super::mcp_runtime::McpRuntime::from_config(plan.config(), plan.runtime_workspace());
    report_progress(progress);
    let mcp_notes = match mcp.as_ref() {
        Some(mcp) => runtime.block_on(mcp.connect()),
        None => Vec::new(),
    };
    report_progress(progress);
    let mut host = runtime.block_on(TurnHost::open_with_mcp(
        plan,
        environment,
        Arc::new(crate::cmd::tool_runtime::HeadlessApproval),
        mcp.as_ref().map(super::mcp_runtime::McpRuntime::catalog),
    ))?;
    report_progress(progress);
    host.activate_extension_composition()?;
    host.push_notes(mcp_notes);

    let (sender, receiver) = event_channel();
    let (outcome, rendered) = runtime.block_on(async {
        tokio::join!(
            async {
                match args.command.as_deref() {
                    Some(command) => {
                        host.drive_command(command, &message, sender.clone())
                            .await?
                    }
                    None => match file_content.as_deref() {
                        Some(content) => {
                            host.drive_content(&message, content, sender.clone())
                                .await?
                        }
                        None => host.drive(&message, sender.clone()).await?,
                    },
                }
                while host
                    .continue_goal_if_idle(zuno_goal::QueuedUserInput::Absent, sender.clone())
                    .await?
                {}
                drop(sender);
                Ok::<(), String>(())
            },
            render_events(receiver, args.format, progress)
        )
    });
    report_progress(progress);
    let shutdown = runtime.block_on(host.shutdown());
    report_progress(progress);
    runtime.block_on(environment.wait_background_jobs());
    report_progress(progress);
    if let Some(mcp) = mcp {
        runtime.block_on(mcp.shutdown());
        report_progress(progress);
    }
    super::turn::finish_with_shutdown(outcome, shutdown)?;
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
    if args.command.is_some() && !args.file.is_empty() {
        return Err("--command and --file cannot be used together".to_owned());
    }
    if args.attach.is_some()
        || args.port.is_some()
        || args.username.is_some()
        || args.password.is_some()
    {
        return Err("--attach/--port/--username/--password require the remote SDK client, which is not available yet".to_owned());
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

fn file_attachment_content(
    prompt: &str,
    files: &[String],
) -> Result<Option<Vec<RequestContentBlock>>, String> {
    if files.is_empty() {
        return Ok(None);
    }
    let mut content = vec![RequestContentBlock::Text {
        text: prompt.to_owned(),
    }];
    for value in files {
        let path = PathBuf::from(value);
        let metadata = std::fs::metadata(&path)
            .map_err(|error| format!("cannot inspect attachment `{}`: {error}", path.display()))?;
        if !metadata.is_file() {
            return Err(format!(
                "attachment `{}` is not a regular file",
                path.display()
            ));
        }
        if let Some(image) = zuno_tui::views::attachment::load_image_file(&path)? {
            content.push(RequestContentBlock::Text {
                text: format!("Attached image: {}", path.display()),
            });
            content.push(RequestContentBlock::Image {
                filename: Some(image.filename),
                media_type: image.media_type,
                data: image.data,
            });
            continue;
        }
        if metadata.len() > u64::try_from(TEXT_ATTACHMENT_MAX_BYTES).unwrap_or(u64::MAX) {
            return Err(format!(
                "text attachment `{}` exceeds the {}-byte limit",
                path.display(),
                TEXT_ATTACHMENT_MAX_BYTES
            ));
        }
        let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
        std::fs::File::open(&path)
            .map_err(|error| format!("cannot read attachment `{}`: {error}", path.display()))?
            .take(u64::try_from(TEXT_ATTACHMENT_MAX_BYTES).unwrap_or(u64::MAX) + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| format!("cannot read attachment `{}`: {error}", path.display()))?;
        if bytes.len() > TEXT_ATTACHMENT_MAX_BYTES {
            return Err(format!(
                "text attachment `{}` exceeds the {}-byte limit",
                path.display(),
                TEXT_ATTACHMENT_MAX_BYTES
            ));
        }
        let body = String::from_utf8(bytes).map_err(|_| {
            format!(
                "attachment `{}` is neither UTF-8 text nor a supported PNG, JPEG, GIF, or WebP image",
                path.display()
            )
        })?;
        let lines = body.lines().count();
        if lines > TEXT_ATTACHMENT_MAX_LINES {
            return Err(format!(
                "text attachment `{}` has {lines} lines, exceeding the {TEXT_ATTACHMENT_MAX_LINES}-line limit",
                path.display()
            ));
        }
        content.push(RequestContentBlock::Text {
            text: format!(
                "--- BEGIN ATTACHED FILE: {} ---\n{body}\n--- END ATTACHED FILE: {} ---",
                path.display(),
                path.display()
            ),
        });
    }
    Ok(Some(content))
}

async fn render_events(
    receiver: tokio::sync::mpsc::Receiver<TurnEvent>,
    format: RunFormat,
    progress: ProgressPulse<'_>,
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
        progress,
    )
    .await
}

async fn render_events_to<Stdout, Stderr>(
    mut receiver: tokio::sync::mpsc::Receiver<TurnEvent>,
    format: RunFormat,
    stdout: &mut Stdout,
    stderr: &mut Stderr,
    stderr_is_terminal: bool,
    progress: ProgressPulse<'_>,
) -> Result<(), String>
where
    Stdout: Write,
    Stderr: Write,
{
    let mut wrote_text = false;
    while let Some(event) = receiver.recv().await {
        report_progress(progress);
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
                TurnEvent::SessionTitleUpdated { title } => {
                    writeln!(stderr, "session titled: {title}").map_err(to_string)?;
                }
                TurnEvent::SessionCommandStarted { command } => {
                    writeln!(stderr, "[{}] started", command.name()).map_err(to_string)?;
                }
                TurnEvent::SessionCommandOutput { content, .. } => {
                    writeln!(stderr, "{content}").map_err(to_string)?;
                }
                TurnEvent::SessionCommandCompleted { command } => {
                    writeln!(stderr, "[{}] completed", command.name()).map_err(to_string)?;
                }
                TurnEvent::SessionCommandFailed { command, message } => {
                    writeln!(stderr, "[{}] failed: {message}", command.name())
                        .map_err(to_string)?;
                }
                TurnEvent::ToolDispatchStarted { display_name, .. } => {
                    writeln!(stderr, "[{display_name}] started").map_err(to_string)?;
                }
                TurnEvent::ToolDispatchCompleted {
                    display_name,
                    title,
                    is_error,
                    ..
                } => writeln!(
                    stderr,
                    "[{display_name}] {}: {title}",
                    if is_error { "failed" } else { "completed" }
                )
                .map_err(to_string)?,
                TurnEvent::ToolDispatchInterrupted {
                    display_name,
                    title,
                    interruption,
                    ..
                } => writeln!(
                    stderr,
                    "[{display_name}] cancelled{}: {title}",
                    if interruption.uncertain() {
                        " (uncertain)"
                    } else {
                        ""
                    }
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

fn report_progress(progress: ProgressPulse<'_>) {
    if let Some(progress) = progress {
        progress();
    }
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
        TurnEvent::SessionMaterialized { session_id, title } => {
            json!({"type":"session_materialized","sessionID":session_id,"title":title})
        }
        TurnEvent::SessionTitleUpdated { title } => {
            json!({"type":"session_title_updated","title":title})
        }
        TurnEvent::SessionCommandStarted { command } => {
            json!({"type":"session_command_started","command":command.name()})
        }
        TurnEvent::SessionCommandOutput { command, content } => {
            json!({"type":"session_command_output","command":command.name(),"content":content})
        }
        TurnEvent::SessionCommandCompleted { command } => {
            json!({"type":"session_command_completed","command":command.name()})
        }
        TurnEvent::SessionCommandFailed { command, message } => {
            json!({"type":"session_command_failed","command":command.name(),"message":message})
        }
        TurnEvent::SkillLoaded { name, source } => {
            json!({"type":"skill_loaded","name":name,"source":source})
        }
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
            estimated_prompt_tokens,
        } => json!({
            "type":"provider_request_started",
            "step":step,
            "messageCount":message_count,
            "estimatedPromptTokens":estimated_prompt_tokens
        }),
        TurnEvent::Provider { step, event } => stream_event_json(step, event),
        TurnEvent::ToolCallStarted {
            step,
            call_id,
            display_name,
            name,
            ui_intent,
        } => json!({
            "type":"tool_call_started",
            "step":step,
            "callID":call_id,
            "name":name,
            "displayName":display_name,
            "uiIntent":ui_intent
        }),
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
            display_name,
            name,
            ..
        } => {
            json!({"type":"tool_dispatch_started","step":step,"callID":call_id,"name":name,"displayName":display_name})
        }
        TurnEvent::ToolDispatchBlocked {
            step,
            call_id,
            kind,
        } => {
            json!({"type":"tool_dispatch_blocked","step":step,"callID":call_id,"kind":kind.as_str()})
        }
        TurnEvent::ToolDispatchInterrupted {
            step,
            call_id,
            display_name,
            name,
            title,
            output,
            interruption,
        } => {
            json!({
                "type":"tool_dispatch_interrupted",
                "step":step,
                "callID":call_id,
                "name":name,
                "displayName":display_name,
                "title":title,
                "output":output,
                "mode":interruption.as_str(),
                "forced":interruption.uncertain(),
                "uncertain":interruption.uncertain(),
            })
        }
        TurnEvent::ToolDispatchCompleted {
            step,
            call_id,
            display_name,
            name,
            title,
            output,
            diff,
            written_paths,
            is_error,
        } => {
            let unified = diff.as_ref().and_then(|diff| diff.unified());
            let files = diff.as_ref().map_or(&[][..], |diff| diff.files());
            json!({"type":"tool_dispatch_completed","step":step,"callID":call_id,"name":name,"displayName":display_name,"title":title,"output":output,"diff":unified,"fileDiffs":files,"writtenPaths":written_paths,"isError":is_error})
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
            request,
        } => json!({
            "type":"turn_interrupted",
            "messageID":assistant_message_id,
            "steps":steps,
            "source":request.map(|request| request.source),
            "reason":request.map(|request| request.reason),
        }),
        TurnEvent::TurnFailed {
            assistant_message_id,
            steps,
            message,
        } => json!({
            "type":"turn_failed",
            "messageID":assistant_message_id,
            "steps":steps,
            "message":message
        }),
    }
}

fn stream_event_json(step: u32, event: StreamEvent) -> Value {
    match event {
        StreamEvent::TextDelta(text) => json!({"type":"text","step":step,"text":text}),
        StreamEvent::ToolUseStart { id, name } => {
            json!({"type":"tool_use_start","step":step,"id":id,"name":name})
        }
        StreamEvent::ToolInputDelta { id, delta } => {
            json!({"type":"tool_input_delta","step":step,"id":id,"delta":delta})
        }
        StreamEvent::ToolUseEnd { id } => {
            json!({"type":"tool_use_end","step":step,"id":id})
        }
        StreamEvent::ToolUseSignature { id, signature } => {
            json!({"type":"tool_use_signature","step":step,"id":id,"signature":format!("{signature:?}")})
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
    use zuno_llm::event::RequestContentBlock;

    #[derive(Clone, Default)]
    struct StallCounter {
        stalled: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl zuno_observability::watchdog::WatchdogSink for StallCounter {
        fn report(&self, report: &zuno_observability::watchdog::WatchdogReport) {
            if report.event == zuno_observability::watchdog::WatchdogEvent::Stalled {
                self.stalled
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }
    }

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

    #[test]
    fn reasoning_flags_are_accepted_by_the_headless_surface() {
        let mut args = run_args();
        args.variant = Some("max".to_owned());
        validate_flags(&args).expect("--variant reaches turn resolution");

        args.variant = None;
        args.thinking = true;
        validate_flags(&args).expect("--thinking reaches turn resolution");
    }

    #[test]
    fn file_attachments_build_typed_text_and_image_content() {
        let root = tempfile::tempdir().expect("attachment fixture");
        let text = root.path().join("notes.txt");
        let image = root.path().join("diagram.png");
        std::fs::write(&text, "portable note\n").expect("text fixture");
        std::fs::write(&image, b"\x89PNG\r\n\x1a\nfixture").expect("image fixture");

        let content = file_attachment_content(
            "inspect these",
            &[
                text.to_string_lossy().into_owned(),
                image.to_string_lossy().into_owned(),
            ],
        )
        .expect("load explicit file attachments")
        .expect("attachments produce rich content");

        assert!(content.iter().any(|block| matches!(
            block,
            RequestContentBlock::Text { text }
                if text.contains("notes.txt") && text.contains("portable note")
        )));
        assert!(content.iter().any(|block| matches!(
            block,
            RequestContentBlock::Image { filename, media_type, data }
                if filename.as_deref() == Some("diagram.png")
                    && media_type == "image/png"
                    && !data.is_empty()
        )));
    }

    #[test]
    fn file_attachments_cannot_be_silently_dropped_by_a_custom_command() {
        let mut args = run_args();
        args.command = Some("review".to_owned());
        args.file.push("diagram.png".to_owned());

        let error = validate_flags(&args).expect_err("command attachments are not wired");
        assert!(error.contains("--command"), "{error}");
        assert!(error.contains("--file"), "{error}");
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
                tokio::join!(producer, render_events(receiver, RunFormat::Default, None));
            produced.expect("producer task");
            rendered.expect("render events");
        })
        .await
        .expect("bounded channel must be consumed concurrently");
    }

    #[tokio::test]
    async fn renderer_events_keep_a_busy_headless_turn_out_of_false_stall() {
        let sink = StallCounter::default();
        let watchdog = zuno_observability::watchdog::Watchdog::spawn_with_sink(
            zuno_observability::watchdog::WatchdogConfig {
                stall_after: std::time::Duration::from_millis(120),
                check_every: std::time::Duration::from_millis(10),
                alive_every: std::time::Duration::from_secs(3_600),
                max_threads_dumped: 4,
                max_stall_backoff: std::time::Duration::from_secs(1),
            },
            sink.clone(),
        );
        let phase = watchdog.phase("test.cli.run.progress");
        let work = watchdog.begin_work(phase);
        let (sender, receiver) = tokio::sync::mpsc::channel(1);
        let producer = async move {
            for index in 0..10 {
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                sender
                    .send(TurnEvent::TurnStarted {
                        session_id: format!("ses_{index}"),
                    })
                    .await
                    .expect("renderer remains connected");
            }
        };
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        {
            let progress = || watchdog.beat(phase);
            let ((), rendered) = tokio::join!(
                producer,
                render_events_to(
                    receiver,
                    RunFormat::Default,
                    &mut stdout,
                    &mut stderr,
                    false,
                    Some(&progress),
                )
            );
            rendered.expect("events render while reporting real progress");
        }
        drop(work);
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;

        assert_eq!(
            sink.stalled.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a headless turn that kept emitting real events was reported as stalled"
        );
        watchdog.shutdown();
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
            None,
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

    #[test]
    fn run_native_session_command_json_uses_the_stable_command_name() {
        assert_eq!(
            event_json(TurnEvent::SessionCommandCompleted {
                command: zuno_engine::session_command::SessionCommand::Compact,
            }),
            json!({"type":"session_command_completed","command":"compact"})
        );
    }

    #[test]
    fn run_pending_tool_json_carries_the_resolved_display_identity() {
        assert_eq!(
            event_json(TurnEvent::ToolCallStarted {
                step: 2,
                call_id: "call-1".to_owned(),
                display_name: "zsh".to_owned(),
                name: "shell".to_owned(),
                ui_intent: zuno_tool::ToolUiIntent::Generic,
            }),
            json!({
                "type":"tool_call_started",
                "step":2,
                "callID":"call-1",
                "name":"shell",
                "displayName":"zsh",
                "uiIntent":"generic"
            })
        );
    }
}
