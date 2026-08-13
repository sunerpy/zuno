use std::io::Write as _;
use std::sync::Arc;

use async_trait::async_trait;
use oc_engine::interrupt::InterruptSignal;
use oc_engine::r#loop::TurnEventSender;
use oc_error::ToolError;
use oc_permission::ReplyKind;
use oc_server::api::{self, ApiState};
use oc_server::{
    AuthConfig, CompatV1State, DEFAULT_EVENT_SUBSCRIBER_CAPACITY, EventService, PermissionRequest,
    QuestionDecision, QuestionRequest, QuestionToolCall, RequestBroker, ServerBuilder,
    ServerConfig, ServerServices, SessionCompactExecution, SessionMutationExecutor,
    SessionMutationFuture, SessionPromptExecution, compat_v1_router, events_router,
};
use oc_tool::{PermissionAsk, PermissionAsker};
use oc_tools::question::{Answer, QuestionAsker};
use uuid::Uuid;

use super::turn::{SessionChoice, TurnHost, TurnOptions, TurnPlan};
use crate::command::ServeArgs;
use crate::environment::StartupEnvironment;

#[derive(Clone, Debug)]
struct ServerSessionMutationExecutor {
    environment: StartupEnvironment,
    requests: RequestBroker,
}

impl ServerSessionMutationExecutor {
    fn new(requests: RequestBroker) -> Self {
        Self {
            environment: StartupEnvironment::resolve(
                &oc_paths::Env::from_process(),
                &crate::command::GlobalOptions::default(),
            ),
            requests,
        }
    }

    async fn open(
        environment: StartupEnvironment,
        session_id: String,
        directory: std::path::PathBuf,
        agent: Option<String>,
        model: Option<oc_server::SessionModelSelection>,
        requests: RequestBroker,
        interrupt: InterruptSignal,
    ) -> Result<TurnHost, String> {
        let options = TurnOptions {
            directory: Some(directory),
            model: model.map(|model| format!("{}/{}", model.provider_id, model.model_id)),
            agent,
            session: SessionChoice::Existing(session_id.clone()),
            title: None,
        };
        let plan = TurnPlan::resolve(&options, &environment).await?;
        let approval: Arc<dyn PermissionAsker> = Arc::new(ServerPermissionAsker {
            requests: requests.clone(),
            session_id,
        });
        let question: Arc<dyn QuestionAsker> = Arc::new(ServerQuestionAsker { requests });
        TurnHost::open_with_interrupt(plan, &environment, approval, Some(question), interrupt)
    }
}

#[derive(Debug)]
struct ServerPermissionAsker {
    requests: RequestBroker,
    session_id: String,
}

#[async_trait]
impl PermissionAsker for ServerPermissionAsker {
    async fn ask(&self, tool: &str, ask: PermissionAsk) -> Result<(), ToolError> {
        let request = PermissionRequest {
            id: format!("per_{}", Uuid::new_v4().simple()),
            session_id: self.session_id.clone(),
            action: ask.permission,
            resources: ask.patterns,
            save: ask.always,
            metadata: ask.metadata,
            source: None,
        };
        match self.requests.ask_permission(request).await {
            ReplyKind::Once | ReplyKind::Always => Ok(()),
            ReplyKind::Reject => Err(ToolError::Denied {
                tool: tool.to_owned(),
            }),
        }
    }
}

#[derive(Debug)]
struct ServerQuestionAsker {
    requests: RequestBroker,
}

#[async_trait]
impl QuestionAsker for ServerQuestionAsker {
    async fn ask(
        &self,
        session_id: &str,
        questions: &[oc_tools::question::QuestionRequest],
        call: Option<(&str, &str)>,
    ) -> Result<Vec<Answer>, ToolError> {
        let questions = questions
            .iter()
            .map(serde_json::to_value)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|source| ToolError::Failed {
                tool: "question".to_owned(),
                source: Box::new(source),
            })?;
        let tool = call.map(|(message_id, call_id)| QuestionToolCall {
            message_id: message_id.to_owned(),
            call_id: call_id.to_owned(),
        });
        let request = QuestionRequest {
            id: format!("que_{}", Uuid::new_v4().simple()),
            session_id: session_id.to_owned(),
            questions,
            tool,
        };
        match self.requests.ask_question(request).await {
            QuestionDecision::Answered(answers) => Ok(answers),
            QuestionDecision::Rejected => Err(ToolError::Denied {
                tool: "question".to_owned(),
            }),
        }
    }
}

impl SessionMutationExecutor for ServerSessionMutationExecutor {
    fn prompt(
        &self,
        request: SessionPromptExecution,
        interrupt: InterruptSignal,
        events: TurnEventSender,
    ) -> SessionMutationFuture {
        let environment = self.environment.clone();
        let requests = self.requests.clone();
        Box::pin(async move {
            let mut host = Self::open(
                environment,
                request.session_id,
                request.directory,
                request.agent,
                request.model,
                requests,
                interrupt,
            )
            .await?;
            let events = host.with_event_hooks(events);
            let outcome = host
                .drive_with_message_id(&request.prompt, Some(&request.message_id), events)
                .await;
            host.shutdown().await;
            outcome
        })
    }

    fn compact(
        &self,
        request: SessionCompactExecution,
        interrupt: InterruptSignal,
    ) -> SessionMutationFuture {
        let environment = self.environment.clone();
        let requests = self.requests.clone();
        Box::pin(async move {
            let mut host = Self::open(
                environment,
                request.session_id,
                request.directory,
                request.agent,
                request.model,
                requests,
                interrupt,
            )
            .await?;
            let outcome = host.compact(request.automatic).await;
            host.shutdown().await;
            outcome
        })
    }
}

pub(super) fn execute(args: &ServeArgs, environment: &StartupEnvironment) -> Result<(), String> {
    if args.mdns {
        return Err("--mdns is not supported by the Rust server runtime yet".to_owned());
    }
    if args.mdns_domain != "opencode.local" {
        return Err("--mdns-domain requires --mdns, which is not supported yet".to_owned());
    }
    if !args.cors.is_empty() {
        return Err("--cors is not supported by the Rust server runtime yet".to_owned());
    }

    let directory_path = std::env::current_dir().map_err(|error| error.to_string())?;
    let directory = directory_path.to_string_lossy().into_owned();
    let auth = AuthConfig::from_env();
    if !auth.required() {
        println!("Warning: OPENCODE_SERVER_PASSWORD is not set; server is unsecured.");
    }

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    runtime.block_on(async {
        let env = environment.resolved();
        let project = oc_paths::project::resolve_project(&directory_path);
        let worktree = project
            .vcs
            .as_ref()
            .map_or(directory_path.as_path(), |_| project.directory.as_path());
        let layout = oc_paths::Layout::resolve(env);
        let plugin_config =
            oc_config::discovery::discover_with(&oc_config::discovery::DiscoveryOptions::new(
                &directory_path,
                Some(worktree),
                env.clone(),
            ))
            .map_err(|error| error.report())?;
        let compat = CompatV1State::new();
        let pool = Arc::new(oc_db::Pool::open_default().map_err(|error| error.to_string())?);
        let events = EventService::new(Arc::clone(&pool), DEFAULT_EVENT_SUBSCRIBER_CAPACITY);
        let config = ServerConfig::default()
            .with_hostname(&args.hostname)
            .with_port(args.port)
            .with_auth(auth)
            .with_default_directory(&directory);
        let state = ApiState::open_default(&directory)
            .map_err(|error| error.to_string())?
            .with_events(events.clone());
        let requests = RequestBroker::with_events(events.clone());
        let services = ServerServices::new(DEFAULT_EVENT_SUBSCRIBER_CAPACITY)
            .with_requests(requests.clone())
            .with_mutations(Arc::new(ServerSessionMutationExecutor::new(requests)));
        let server = ServerBuilder::new(config)
            .with_services(services)
            .with_routes(
                api::router(state.clone())
                    .merge(events_router(events))
                    .merge(compat_v1_router(compat.clone(), state)),
            )
            .bind()
            .await
            .map_err(|error| error.to_string())?;
        let plugin_server_url = reqwest::Url::parse(&format!("http://{}", server.local_addr()))
            .map_err(|error| error.to_string())?;
        let plugins = super::plugin_runtime::PluginRuntime::load(
            &plugin_config,
            &project,
            &directory_path,
            worktree,
            &layout,
            env.flag(crate::OPENCODE_PURE),
            super::plugin_runtime::PluginRuntimeTarget::server_with_stdio(
                "serve",
                plugin_server_url,
            ),
        )
        .await
        .map(Arc::new);
        if let Some(plugins) = &plugins {
            compat.set_provider_oauth_backend(
                Arc::clone(plugins) as Arc<dyn oc_server::ProviderOAuthBackend>
            );
        }
        println!(
            "opencode server listening on http://{}",
            server.local_addr()
        );
        std::io::stdout()
            .flush()
            .map_err(|error| error.to_string())?;
        let result = server.serve().await.map_err(|error| error.to_string());
        if let Some(plugins) = plugins {
            plugins.shutdown().await;
        }
        result
    })
}
