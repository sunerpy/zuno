use std::io::{IsTerminal as _, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use oc_auth::Credential;
use oc_engine::dispatch::ToolRegistryDispatcher;
use oc_engine::interrupt::InterruptSignal;
use oc_engine::r#loop::{
    AgentModelResolver, ResolvedAgent, ResolvedModel as EngineModel, RunTurnRequest, TurnContext,
    TurnEvent, event_channel, run_turn,
};
use oc_llm::cache::{DynamicContext, McpToolStatus};
use oc_llm::catalog::{Catalog, CatalogSource, ResolveInput};
use oc_llm::event::{ConnectionPhase, StreamEvent};
use oc_llm::registry::{ApiSurface, ProviderRegistry, Spec};
use oc_provider_compatible::{ReqwestTransport, Transport, factory};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::command::{RunArgs, RunFormat};
use crate::environment::StartupEnvironment;

const COMPATIBLE_PROVIDER: &str = "openai-compatible";
const DEFAULT_AGENT: &str = "build";
const DEFAULT_MAX_STEPS: u32 = 100;
const OPENCODE_ENABLE_EXPERIMENTAL_MODELS: &str = "OPENCODE_ENABLE_EXPERIMENTAL_MODELS";

pub(super) fn execute(args: &RunArgs, environment: &StartupEnvironment) -> Result<(), String> {
    validate_flags(args)?;
    let message = prompt(args)?;
    let directory = args
        .dir
        .as_deref()
        .map(PathBuf::from)
        .map_or_else(std::env::current_dir, Ok)
        .map_err(to_string)?;
    let env = environment.resolved();
    let project = oc_paths::project::resolve_project(&directory);
    let worktree = project.vcs.as_ref().map(|_| project.directory.as_path());
    let layout = oc_paths::Layout::resolve(env);
    let config = oc_config::discovery::discover_with(&oc_config::discovery::DiscoveryOptions::new(
        &directory,
        worktree,
        env.clone(),
    ))
    .map_err(to_string)?;
    let credentials = oc_auth::AuthStore::resolve(&layout, env)
        .all()
        .map_err(to_string)?
        .entries;
    let catalog_source = CatalogSource::resolve(env, &layout);
    let runtime = runtime()?;
    let document = runtime.block_on(catalog_source.load()).map_err(to_string)?;
    let input = ResolveInput::new()
        .with_config(&config)
        .with_credentials(credentials.clone())
        .with_env(
            env.iter()
                .map(|(key, value)| (key.to_owned(), value.to_owned()))
                .collect(),
        )
        .with_experimental_models(env.flag(OPENCODE_ENABLE_EXPERIMENTAL_MODELS));
    let catalog = Catalog::resolve(&document, &input);

    let agents = oc_catalog::agent::load(&directory, worktree, env).map_err(to_string)?;
    let agent_name = args.agent.as_deref().unwrap_or(DEFAULT_AGENT);
    let selected_agent = agents
        .iter()
        .find(|entry| entry.name == agent_name)
        .ok_or_else(|| format!("Agent not found: {agent_name}"))?;
    let requested_model = args.model.as_deref().or(selected_agent.model.as_deref());
    let (provider_id, model_id, catalog_model) = select_model(&catalog, requested_model)?;
    if !supports_compatible_transport(&catalog_model.api.npm) {
        return Err(format!(
            "model {provider_id}/{model_id} uses transport {}, but this headless run path currently supports OpenAI-compatible transports",
            catalog_model.api.npm
        ));
    }
    let credential = credentials.get(&provider_id).map(credential_value);
    let spec = model_spec(catalog_model);
    let resolver = Resolver {
        requested_agent: selected_agent.name.clone(),
        system_prompt: selected_agent.prompt.clone().unwrap_or_default(),
        max_steps: selected_agent
            .steps
            .map_or(DEFAULT_MAX_STEPS, std::num::NonZeroU32::get),
        requested_provider: provider_id.clone(),
        requested_model: model_id.clone(),
        wire_model: catalog_model.api.id.clone(),
        spec,
    };

    let mut providers = ProviderRegistry::new();
    let transport: Arc<dyn Transport> = Arc::new(ReqwestTransport::new(&provider_id));
    providers.register_fallible(
        COMPATIBLE_PROVIDER,
        factory(transport, move |_| credential.clone()),
    );

    let mut connection = oc_db::open_default().map_err(to_string)?;
    oc_db::migration::apply(&mut connection).map_err(to_string)?;
    let now = oc_db::message::now_millis();
    ensure_project(&connection, &project, now)?;
    let session = resolve_session(
        &mut connection,
        args,
        &project,
        &directory,
        &provider_id,
        &model_id,
        agent_name,
        now,
    )?;
    persist_user_message(
        &connection,
        &session.id,
        agent_name,
        &provider_id,
        &model_id,
        &message,
        now,
    )?;

    let interrupt = InterruptSignal::new();
    let runtime_tools = crate::cmd::tool_runtime::assemble(
        &directory,
        worktree,
        env,
        &config,
        selected_agent,
        &provider_id,
        &model_id,
    )?;
    let dispatcher = ToolRegistryDispatcher::new(
        runtime_tools.tools,
        runtime_tools.rules,
        Arc::new(crate::cmd::tool_runtime::HeadlessApproval),
        InterruptSignal::new(),
        McpToolStatus::Ready,
    );
    let (sender, receiver) = event_channel();
    let turn = run_turn(
        RunTurnRequest::new(
            session.id,
            Uuid::new_v4().simple().to_string(),
            DynamicContext::default(),
        ),
        TurnContext::new(
            &mut connection,
            &providers,
            &resolver,
            &dispatcher,
            &interrupt,
        ),
        sender,
    );
    let (outcome, rendered) =
        runtime.block_on(async { tokio::join!(turn, render_events(receiver, args.format)) });
    outcome.map_err(to_string)?;
    rendered?;
    Ok(())
}

fn validate_flags(args: &RunArgs) -> Result<(), String> {
    if args.command.is_some() {
        return Err(
            "--command templates are not supported by the headless Rust run path yet".to_owned(),
        );
    }
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
    if args.interactive || args.auto {
        return Err(
            "--interactive and --auto require the TUI loop and are not available in headless run"
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

fn select_model<'a>(
    catalog: &'a Catalog,
    requested: Option<&str>,
) -> Result<(String, String, &'a oc_llm::catalog::ResolvedModel), String> {
    if let Some(requested) = requested {
        let (provider_id, model_id) = requested
            .split_once('/')
            .ok_or_else(|| format!("model must be provider/model, got {requested:?}"))?;
        let model = catalog
            .model(provider_id, model_id)
            .ok_or_else(|| format!("Model not found: {requested}"))?;
        return Ok((provider_id.to_owned(), model_id.to_owned(), model));
    }

    for provider_id in catalog.provider_ids() {
        let provider = catalog
            .provider(provider_id)
            .expect("provider_ids only returns catalog providers");
        let mut model_ids: Vec<&str> = provider.models.keys().map(String::as_str).collect();
        model_ids.sort_by(|left, right| oc_llm::catalog::collate::compare(left, right));
        if let Some(model_id) = model_ids.into_iter().next() {
            let model = provider
                .models
                .get(model_id)
                .expect("model id came from provider models");
            return Ok((provider_id.to_owned(), model_id.to_owned(), model));
        }
    }
    Err("no available model; configure a provider credential or provider block".to_owned())
}

fn supports_compatible_transport(npm: &str) -> bool {
    matches!(
        npm,
        "@ai-sdk/openai-compatible" | "@ai-sdk/openai" | "@openrouter/ai-sdk-provider"
    )
}

fn model_spec(model: &oc_llm::catalog::ResolvedModel) -> Spec {
    let mut spec = Spec::new(COMPATIBLE_PROVIDER).with_surface(ApiSurface::Chat);
    if !model.api.url.is_empty() {
        spec = spec.with_base_url(&model.api.url);
    }
    for (name, value) in &model.headers {
        spec = spec.with_header(name, value);
    }
    for (name, value) in &model.options {
        spec = spec.with_option(name, value.clone());
    }
    spec
}

fn credential_value(credential: &Credential) -> String {
    match credential {
        Credential::Api { key, .. } => key.expose().to_owned(),
        Credential::Oauth { access, .. } => access.expose().to_owned(),
        Credential::WellKnown { token, .. } => token.expose().to_owned(),
    }
}

struct Resolver {
    requested_agent: String,
    system_prompt: String,
    max_steps: u32,
    requested_provider: String,
    requested_model: String,
    wire_model: String,
    spec: Spec,
}

impl AgentModelResolver for Resolver {
    fn resolve_agent(&self, requested: &str) -> Option<ResolvedAgent> {
        (requested == self.requested_agent).then(|| {
            ResolvedAgent::new(
                self.requested_agent.clone(),
                self.system_prompt.clone(),
                self.max_steps,
            )
        })
    }

    fn resolve_model(&self, provider_id: &str, model_id: &str) -> Option<EngineModel> {
        (provider_id == self.requested_provider && model_id == self.requested_model)
            .then(|| EngineModel::new(self.spec.clone(), self.wire_model.clone(), ApiSurface::Chat))
    }
}

fn ensure_project(
    connection: &rusqlite::Connection,
    project: &oc_paths::project::ResolvedProject,
    now: i64,
) -> Result<(), String> {
    connection
        .execute(
            "INSERT INTO project \
             (id, worktree, vcs, time_created, time_updated, sandboxes) \
             VALUES (?1, ?2, ?3, ?4, ?4, '[]') \
             ON CONFLICT (id) DO UPDATE SET \
               worktree = excluded.worktree, \
               vcs = excluded.vcs, \
               time_updated = excluded.time_updated",
            (
                project.id.as_str(),
                project.directory.to_string_lossy().as_ref(),
                project.vcs.as_ref().map(|_| "git"),
                now,
            ),
        )
        .map_err(to_string)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn resolve_session(
    connection: &mut rusqlite::Connection,
    args: &RunArgs,
    project: &oc_paths::project::ResolvedProject,
    directory: &Path,
    provider_id: &str,
    model_id: &str,
    agent: &str,
    now: i64,
) -> Result<oc_db::session::Session, String> {
    if let Some(session_id) = &args.session {
        return oc_db::session::get(connection, session_id).map_err(to_string);
    }
    if args.r#continue {
        return oc_db::session::list(
            connection,
            &oc_db::session::ListQuery::directory(directory.to_string_lossy())
                .active_only()
                .with_limit(1),
        )
        .map_err(to_string)?
        .into_iter()
        .next()
        .ok_or_else(|| "no session found to continue in the current directory".to_owned());
    }

    let session_id = prefixed_id("ses");
    let title = args
        .title
        .clone()
        .unwrap_or_else(|| "New session".to_owned());
    let mut input = oc_db::session::SessionCreate::new(
        &session_id,
        Uuid::new_v4().simple().to_string(),
        &project.id,
        project.directory.to_string_lossy().into_owned(),
        directory.to_string_lossy().into_owned(),
        title,
        crate::COMPATIBILITY_VERSION,
    )
    .at(now);
    input.agent = Some(agent.to_owned());
    input.model = Some(json!({"providerID": provider_id, "modelID": model_id}).to_string());
    let transaction = connection.transaction().map_err(to_string)?;
    let creation = oc_db::session::create(&transaction, &input).map_err(to_string)?;
    transaction.commit().map_err(to_string)?;
    Ok(creation.into_session())
}

fn persist_user_message(
    connection: &rusqlite::Connection,
    session_id: &str,
    agent: &str,
    provider_id: &str,
    model_id: &str,
    text: &str,
    now: i64,
) -> Result<(), String> {
    let message_id = prefixed_id("msg");
    let message = oc_db::message::MessageRecord::from_json(json!({
        "id": message_id,
        "sessionID": session_id,
        "role": "user",
        "time": {"created": now},
        "agent": agent,
        "model": {"providerID": provider_id, "modelID": model_id}
    }))
    .map_err(to_string)?;
    let part = oc_db::message::PartRecord::from_json(
        json!({
            "id": prefixed_id("prt"),
            "sessionID": session_id,
            "messageID": message.id,
            "type": "text",
            "text": text
        }),
        now,
    )
    .map_err(to_string)?;
    let store = oc_db::message::MessageStore::new(connection);
    store.put_message_at(&message, now).map_err(to_string)?;
    store.put_part_at(&part, now).map_err(to_string)?;
    Ok(())
}

async fn render_events(
    mut receiver: tokio::sync::mpsc::Receiver<TurnEvent>,
    format: RunFormat,
) -> Result<(), String> {
    let mut wrote_text = false;
    while let Some(event) = receiver.recv().await {
        match format {
            RunFormat::Default => match event {
                TurnEvent::Provider {
                    event: StreamEvent::TextDelta(text),
                    ..
                } => {
                    print!("{text}");
                    std::io::stdout().flush().map_err(to_string)?;
                    wrote_text = true;
                }
                TurnEvent::Provider {
                    event: StreamEvent::Error { message, .. },
                    ..
                } => eprintln!("{message}"),
                TurnEvent::ToolDispatchStarted { name, .. } => eprintln!("[{name}] started"),
                TurnEvent::ToolDispatchCompleted {
                    name,
                    title,
                    is_error,
                    ..
                } => eprintln!(
                    "[{name}] {}: {title}",
                    if is_error { "failed" } else { "completed" }
                ),
                _ => {}
            },
            RunFormat::Json => println!("{}", event_json(event)),
        }
    }
    if format == RunFormat::Default && wrote_text {
        println!();
    }
    Ok(())
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
            is_error,
        } => {
            json!({"type":"tool_dispatch_completed","step":step,"callID":call_id,"name":name,"title":title,"output":output,"isError":is_error})
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
        } => {
            json!({"type":"token_usage","step":step,"inputTokens":input_tokens,"outputTokens":output_tokens,"cacheReadInputTokens":cache_read_input_tokens,"cacheWriteInputTokens":cache_write_input_tokens})
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

fn prefixed_id(prefix: &str) -> String {
    format!("{prefix}_{}", Uuid::new_v4().simple())
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
    fn model_selection_splits_only_the_provider_prefix() {
        let document = serde_json::from_str(
            r#"{"anyapi":{"id":"anyapi","name":"AnyAPI","env":[],"models":{"openai/gpt":{"id":"openai/gpt","name":"GPT","limit":{"context":1,"output":1}}}}}"#,
        )
        .expect("catalog document");
        let config = serde_json::from_str(r#"{"provider":{"anyapi":{}}}"#).expect("config");
        let catalog = Catalog::resolve(&document, &ResolveInput::new().with_config(&config));
        let (provider, model, _) =
            select_model(&catalog, Some("anyapi/openai/gpt")).expect("nested model id");
        assert_eq!(provider, "anyapi");
        assert_eq!(model, "openai/gpt");
    }

    #[test]
    fn unsupported_remote_and_session_flags_are_rejected_before_side_effects() {
        let mut args = run_args();
        args.r#continue = true;
        args.session = Some("ses_x".to_owned());
        assert!(validate_flags(&args).is_err());
    }

    #[test]
    fn new_session_and_user_message_are_persisted_together() {
        let mut connection =
            oc_db::open::open(&oc_paths::DbLocation::Memory).expect("open memory database");
        oc_db::migration::apply(&mut connection).expect("apply schema");
        let project = oc_paths::project::ResolvedProject {
            previous: None,
            id: "project-run-test".to_owned(),
            directory: PathBuf::from("/workspace"),
            vcs: None,
        };
        let now = 1_780_000_000_000;
        ensure_project(&connection, &project, now).expect("persist project");
        let session = resolve_session(
            &mut connection,
            &run_args(),
            &project,
            Path::new("/workspace"),
            "provider",
            "model",
            "build",
            now,
        )
        .expect("create session");
        persist_user_message(
            &connection,
            &session.id,
            "build",
            "provider",
            "model",
            "hello",
            now,
        )
        .expect("persist prompt");

        let store = oc_db::message::MessageStore::new(&connection);
        let messages = store
            .messages_for_session(&session.id)
            .expect("load messages");
        assert_eq!(messages.len(), 1);
        let grouped = store
            .parts_by_message(&[messages[0].id.clone()])
            .expect("load message parts");
        let parts = grouped
            .get(&messages[0].id)
            .expect("parts grouped under the message");
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].data["text"], "hello");
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
}
