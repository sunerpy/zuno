// allow: SIZE_OK — exhaustive conformance table for the authoritative plugin hook union.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use oc_config::Config;
use oc_config::schema::ordered::OrderedMap;
use oc_db::message::{MessageRecord, MessageRole};
use oc_engine::r#loop::TurnEvent;
use oc_error::{BoxSource, ToolError};
use oc_llm::catalog::availability::Availability;
use oc_llm::catalog::models_dev::CatalogStatus;
use oc_llm::catalog::resolved::{
    ModelApi, ModelCapabilities, ModelCost, ModelLimit, ResolvedModel, ResolvedProvider,
};
use oc_llm::event::{Message, Role};
use oc_permission::PermissionRequest;
use oc_plugin::*;
use oc_tool::{Tool, ToolContext, ToolDefinition, ToolOutput};
use serde_json::json;

const EXPECTED: [HookName; 21] = HookName::ALL;

struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn id(&self) -> &str {
        "echo"
    }

    fn description(&self) -> &str {
        "echo"
    }

    fn raw_parameters_schema(&self) -> serde_json::Value {
        json!({ "type": "object" })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::text("echo", args.to_string()))
    }
}

struct RecordingPlugin {
    manifest: PluginManifest,
    seen: Arc<Mutex<Vec<HookName>>>,
}

impl RecordingPlugin {
    fn new(seen: Arc<Mutex<Vec<HookName>>>) -> Self {
        Self {
            manifest: PluginManifest::new("recording", EXPECTED.to_vec()).expect("manifest"),
            seen,
        }
    }

    fn record(&self, hook: HookName) {
        self.seen.lock().expect("seen lock").push(hook);
    }
}

#[async_trait]
impl Plugin for RecordingPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    fn tools(&self) -> PluginTools {
        self.record(HookName::Tool);
        OrderedMap::from_iter([("echo".to_owned(), Arc::new(EchoTool) as Arc<dyn Tool>)])
    }

    fn auth(&self) -> Option<AuthHook> {
        self.record(HookName::Auth);
        Some(AuthHook {
            provider: "recording".to_owned(),
            loader: None,
            methods: Vec::new(),
        })
    }

    fn provider(&self) -> Option<ProviderHook> {
        self.record(HookName::Provider);
        Some(ProviderHook {
            id: "recording".to_owned(),
            models: None,
        })
    }

    async fn call(&self, hook: &mut HookInvocation<'_>) -> Result<(), BoxSource> {
        self.record(hook.name());
        match hook {
            HookInvocation::Dispose => {}
            HookInvocation::Event { event } => {
                assert!(
                    matches!(event, TurnEvent::TurnStarted { session_id } if session_id == "ses")
                );
            }
            HookInvocation::Config { config } => config.username = Some("plugin-user".to_owned()),
            HookInvocation::Tool { .. }
            | HookInvocation::Auth { .. }
            | HookInvocation::Provider { .. } => panic!("resource hooks bypass call"),
            HookInvocation::ChatMessage { input, output } => {
                assert_eq!(input.session_id, "ses");
                output.message.role = MessageRole::Assistant;
            }
            HookInvocation::ChatParams { input, output } => {
                assert_eq!(input.model.id, "model");
                output.temperature += 1.0;
            }
            HookInvocation::ChatHeaders { input, output } => {
                assert_eq!(input.provider.info.id, "provider");
                output
                    .headers
                    .insert("x-plugin".to_owned(), "yes".to_owned());
            }
            HookInvocation::PermissionAsk { input, output } => {
                assert_eq!(input.request.permission, "read");
                output.status = PermissionStatus::Allow;
            }
            HookInvocation::CommandExecuteBefore { input, output } => {
                assert_eq!(input.command, "build");
                output.parts.clear();
            }
            HookInvocation::ToolExecuteBefore { input, output } => {
                assert_eq!(input.tool, "echo");
                output.args = json!({ "changed": true });
            }
            HookInvocation::ShellEnv { input, output } => {
                assert_eq!(input.cwd, "/work");
                output.env.insert("PLUGIN".to_owned(), "1".to_owned());
            }
            HookInvocation::ToolExecuteAfter { input, output } => {
                assert_eq!(input.call_id, "call");
                output.title = "changed".to_owned();
            }
            HookInvocation::ChatMessagesTransform { output } => output.messages.reverse(),
            HookInvocation::ChatSystemTransform { input, output } => {
                assert_eq!(input.model.provider_id, "provider");
                output.system.push("plugin system".to_owned());
            }
            HookInvocation::ProviderSmallModel { input, output } => {
                assert_eq!(input.provider.id, "provider");
                output.model = Some(model());
            }
            HookInvocation::SessionCompacting { input, output } => {
                assert_eq!(input.session_id, "ses");
                output.context.push("plugin context".to_owned());
            }
            HookInvocation::CompactionAutocontinue { input, output } => {
                assert!(input.overflow);
                output.enabled = false;
            }
            HookInvocation::TextComplete { input, output } => {
                assert_eq!(input.part_id, "part");
                output.text.push('!');
            }
            HookInvocation::ToolDefinition { input, output } => {
                assert_eq!(input.tool_id, "echo");
                output.description = "changed".to_owned();
            }
        }
        Ok(())
    }
}

struct TemperaturePlugin {
    manifest: PluginManifest,
    operation: TemperatureOperation,
}

enum TemperatureOperation {
    AddOne,
    TimesTen,
}

#[async_trait]
impl Plugin for TemperaturePlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    async fn call(&self, hook: &mut HookInvocation<'_>) -> Result<(), BoxSource> {
        if let HookInvocation::ChatParams { output, .. } = hook {
            output.temperature = match self.operation {
                TemperatureOperation::AddOne => output.temperature + 1.0,
                TemperatureOperation::TimesTen => output.temperature * 10.0,
            };
        }
        Ok(())
    }
}

fn model() -> ResolvedModel {
    ResolvedModel {
        id: "model".to_owned(),
        provider_id: "provider".to_owned(),
        name: "Model".to_owned(),
        family: "test".to_owned(),
        release_date: String::new(),
        status: CatalogStatus::Active,
        api: ModelApi::default(),
        capabilities: ModelCapabilities::default(),
        cost: ModelCost::default(),
        limit: ModelLimit::default(),
        options: serde_json::Map::new(),
        headers: BTreeMap::new(),
        variants: BTreeMap::new(),
    }
}

fn provider() -> ResolvedProvider {
    ResolvedProvider {
        id: "provider".to_owned(),
        name: "Provider".to_owned(),
        env: Vec::new(),
        options: serde_json::Map::new(),
        availability: Availability::none(),
        models: BTreeMap::new(),
    }
}

fn context<'a>(model: &'a ResolvedModel, provider: &'a ProviderContext) -> ChatContext<'a> {
    ChatContext {
        session_id: "ses",
        agent: "build",
        model,
        provider,
        message: Message::new(Role::User, "hello"),
    }
}

#[tokio::test]
async fn all_authoritative_hooks_dispatch_with_their_typed_payloads() {
    // Given
    let seen = Arc::new(Mutex::new(Vec::new()));
    let bus = HookBus::new(vec![Arc::new(RecordingPlugin::new(Arc::clone(&seen)))]);
    let model = model();
    let provider = provider();
    let provider_context = ProviderContext {
        source: ProviderSource::Config,
        info: provider.clone(),
        options: serde_json::Map::new(),
    };
    let chat_context = context(&model, &provider_context);

    // When
    bus.dispatch(HookInvocation::Dispose)
        .await
        .expect("dispose");
    bus.dispatch(HookInvocation::Event {
        event: &TurnEvent::TurnStarted {
            session_id: "ses".to_owned(),
        },
    })
    .await
    .expect("event");
    let mut config = Config::default();
    bus.dispatch(HookInvocation::Config {
        config: &mut config,
    })
    .await
    .expect("config");
    let mut tools = PluginTools::new();
    bus.dispatch(HookInvocation::Tool { output: &mut tools })
        .await
        .expect("tool");
    let mut auth = Vec::new();
    bus.dispatch(HookInvocation::Auth { output: &mut auth })
        .await
        .expect("auth");
    let mut providers = Vec::new();
    bus.dispatch(HookInvocation::Provider {
        output: &mut providers,
    })
    .await
    .expect("provider");
    let mut message = ChatMessageOutput {
        message: MessageRecord::from_json(json!({
            "id": "message",
            "sessionID": "ses",
            "role": "user",
            "time": {"created": 1},
            "agent": "build",
            "model": {"providerID": "provider", "modelID": "model"}
        }))
        .expect("user message"),
        parts: Vec::new(),
    };
    bus.dispatch(HookInvocation::ChatMessage {
        input: &ChatMessageInput {
            session_id: "ses",
            agent: Some("build"),
            model: Some(ModelSelection {
                provider_id: "provider",
                model_id: "model",
            }),
            message_id: Some("message"),
            variant: None,
        },
        output: &mut message,
    })
    .await
    .expect("chat.message");
    let mut params = ChatParamsOutput::default();
    bus.dispatch(HookInvocation::ChatParams {
        input: &chat_context,
        output: &mut params,
    })
    .await
    .expect("chat.params");
    let mut headers = ChatHeadersOutput::default();
    bus.dispatch(HookInvocation::ChatHeaders {
        input: &chat_context,
        output: &mut headers,
    })
    .await
    .expect("chat.headers");
    let permission = PermissionRequest {
        id: "perm".to_owned(),
        session_id: "ses".to_owned(),
        permission: "read".to_owned(),
        patterns: vec!["*".to_owned()],
        metadata: serde_json::Map::new(),
        always: Vec::new(),
        tool: None,
    };
    let mut permission_output = PermissionAskOutput::default();
    bus.dispatch(HookInvocation::PermissionAsk {
        input: &PermissionAskInput {
            request: &permission,
        },
        output: &mut permission_output,
    })
    .await
    .expect("permission.ask");
    let mut command_output = CommandExecuteBeforeOutput::default();
    bus.dispatch(HookInvocation::CommandExecuteBefore {
        input: &CommandExecuteBeforeInput {
            command: "build",
            session_id: "ses",
            arguments: "--release",
        },
        output: &mut command_output,
    })
    .await
    .expect("command.execute.before");
    let mut before_output = ToolExecuteBeforeOutput {
        args: json!({ "value": 1 }),
    };
    bus.dispatch(HookInvocation::ToolExecuteBefore {
        input: &ToolExecuteBeforeInput {
            tool: "echo",
            session_id: "ses",
            call_id: "call",
        },
        output: &mut before_output,
    })
    .await
    .expect("tool.execute.before");
    let mut env_output = ShellEnvOutput::default();
    bus.dispatch(HookInvocation::ShellEnv {
        input: &ShellEnvInput {
            cwd: "/work",
            session_id: Some("ses"),
            call_id: Some("call"),
        },
        output: &mut env_output,
    })
    .await
    .expect("shell.env");
    let mut after_output = ToolOutput::text("echo", "ok");
    bus.dispatch(HookInvocation::ToolExecuteAfter {
        input: &ToolExecuteAfterInput {
            tool: "echo",
            session_id: "ses",
            call_id: "call",
            args: &before_output.args,
        },
        output: &mut after_output,
    })
    .await
    .expect("tool.execute.after");
    let mut messages_output = ChatMessagesTransformOutput {
        messages: vec![
            MessageWithParts {
                info: Message::new(Role::User, "one"),
                parts: Vec::new(),
            },
            MessageWithParts {
                info: Message::new(Role::Assistant, "two"),
                parts: Vec::new(),
            },
        ],
    };
    bus.dispatch(HookInvocation::ChatMessagesTransform {
        output: &mut messages_output,
    })
    .await
    .expect("messages transform");
    let mut system_output = ChatSystemTransformOutput::default();
    bus.dispatch(HookInvocation::ChatSystemTransform {
        input: &ChatSystemTransformInput {
            session_id: Some("ses"),
            model: &model,
        },
        output: &mut system_output,
    })
    .await
    .expect("system transform");
    let mut small_model_output = ProviderSmallModelOutput::default();
    bus.dispatch(HookInvocation::ProviderSmallModel {
        input: &ProviderSmallModelInput {
            provider: &provider,
        },
        output: &mut small_model_output,
    })
    .await
    .expect("small model");
    let mut compacting_output = SessionCompactingOutput::default();
    bus.dispatch(HookInvocation::SessionCompacting {
        input: &SessionCompactingInput { session_id: "ses" },
        output: &mut compacting_output,
    })
    .await
    .expect("compacting");
    let mut autocontinue_output = CompactionAutocontinueOutput { enabled: true };
    bus.dispatch(HookInvocation::CompactionAutocontinue {
        input: &CompactionAutocontinueInput {
            context: &chat_context,
            overflow: true,
        },
        output: &mut autocontinue_output,
    })
    .await
    .expect("autocontinue");
    let mut text_output = TextCompleteOutput {
        text: "done".to_owned(),
    };
    bus.dispatch(HookInvocation::TextComplete {
        input: &TextCompleteInput {
            session_id: "ses",
            message_id: "message",
            part_id: "part",
        },
        output: &mut text_output,
    })
    .await
    .expect("text complete");
    let mut definition = ToolDefinition {
        id: "echo".to_owned(),
        description: "echo".to_owned(),
        parameters: json!({ "type": "object" }),
    };
    bus.dispatch(HookInvocation::ToolDefinition {
        input: &ToolDefinitionInput { tool_id: "echo" },
        output: &mut definition,
    })
    .await
    .expect("tool definition");

    // Then
    assert_eq!(*seen.lock().expect("seen lock"), EXPECTED);
    assert_eq!(config.username.as_deref(), Some("plugin-user"));
    assert_eq!(tools.len(), 1);
    assert_eq!(auth[0].provider, "recording");
    assert_eq!(providers[0].id, "recording");
    assert_eq!(params.temperature, 1.0);
    assert_eq!(
        headers.headers.get("x-plugin").map(String::as_str),
        Some("yes")
    );
    assert_eq!(definition.description, "changed");
}

#[tokio::test]
async fn hooks_apply_output_mutations_in_configuration_order() {
    // Given
    let manifest =
        || PluginManifest::new("temperature", vec![HookName::ChatParams]).expect("manifest");
    let bus = HookBus::new(vec![
        Arc::new(TemperaturePlugin {
            manifest: manifest(),
            operation: TemperatureOperation::AddOne,
        }),
        Arc::new(TemperaturePlugin {
            manifest: manifest(),
            operation: TemperatureOperation::TimesTen,
        }),
    ]);
    let model = model();
    let provider_context = ProviderContext {
        source: ProviderSource::Config,
        info: provider(),
        options: serde_json::Map::new(),
    };
    let input = context(&model, &provider_context);
    let mut output = ChatParamsOutput {
        temperature: 1.0,
        ..ChatParamsOutput::default()
    };

    // When
    bus.dispatch(HookInvocation::ChatParams {
        input: &input,
        output: &mut output,
    })
    .await
    .expect("dispatch");

    // Then
    assert_eq!(output.temperature, 20.0);
}
