use std::io::Write as _;
use std::sync::Arc;

use async_trait::async_trait;
use uuid::Uuid;
use zuno_engine::r#loop::TurnEventSender;
use zuno_engine::status::{SessionRunGuard, SessionRunRegistry};
use zuno_error::ToolError;
use zuno_permission::ReplyKind;
use zuno_server::api::{self, ApiState};
use zuno_server::{
    AuthConfig, CompatV1State, DEFAULT_EVENT_SUBSCRIBER_CAPACITY, EventService, PermissionRequest,
    QuestionDecision, QuestionRequest, QuestionToolCall, RequestBroker, ServerBuilder,
    ServerConfig, ServerServices, SessionCompactExecution, SessionMutationExecutor,
    SessionMutationFuture, SessionPromptExecution, compat_v1_router, events_router,
};
use zuno_tool::{PermissionAsk, PermissionAsker};
use zuno_tools::question::{Answer, QuestionAsker};

use super::turn::{SessionChoice, TurnHost, TurnOptions, TurnPlan};
use crate::command::ServeArgs;
use crate::environment::StartupEnvironment;

#[derive(Clone)]
struct ServerSessionMutationExecutor {
    environment: StartupEnvironment,
    requests: RequestBroker,
    runs: SessionRunRegistry,
    /// The one MCP catalog every session on this server shares.
    ///
    /// A host is built per request here, so building a *catalog* per request would
    /// spawn and tear down every configured MCP server on every prompt — for a stdio
    /// server that is a subprocess launch and an `initialize` round trip per turn. The
    /// controller that owns the transports lives in [`execute`] for the server's whole
    /// lifetime; each host only reads the merged tool list out of this clone.
    mcp: Option<zuno_mcp::Catalog>,
}

impl ServerSessionMutationExecutor {
    fn new(
        requests: RequestBroker,
        runs: SessionRunRegistry,
        mcp: Option<zuno_mcp::Catalog>,
    ) -> Self {
        Self {
            environment: StartupEnvironment::resolve(
                &zuno_paths::Env::from_process(),
                &crate::command::GlobalOptions::default(),
            ),
            requests,
            runs,
            mcp,
        }
    }

    /// Open a host for one request.
    ///
    /// Takes `self` by value rather than the executor's four fields as arguments: the
    /// environment, the request broker, the run registry and the MCP catalog all
    /// belong to the executor, and unpacking them at each call site only to pass them
    /// back in is what pushed this past the argument limit when MCP was added.
    async fn open(
        self,
        session_id: String,
        directory: std::path::PathBuf,
        agent: Option<String>,
        model: Option<zuno_server::SessionModelSelection>,
    ) -> Result<TurnHost, String> {
        let options = TurnOptions {
            directory: Some(directory),
            model: model.map(|model| format!("{}/{}", model.provider_id, model.model_id)),
            agent,
            session: SessionChoice::Existing(session_id.clone()),
            title: None,
            effort: None,
        };
        let plan = TurnPlan::resolve(&options, &self.environment).await?;
        let approval: Arc<dyn PermissionAsker> = Arc::new(ServerPermissionAsker {
            requests: self.requests.clone(),
            session_id,
        });
        let question: Arc<dyn QuestionAsker> = Arc::new(ServerQuestionAsker {
            requests: self.requests,
        });
        TurnHost::open_with_runtime_and_mcp(
            plan,
            &self.environment,
            approval,
            Some(question),
            self.runs,
            self.mcp,
        )
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
        questions: &[zuno_tools::question::QuestionRequest],
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

/// Hand-written because [`zuno_mcp::Catalog`] is deliberately not [`Debug`].
///
/// Deriving it would need a `Debug` on the catalog, and the useful fact here is
/// whether MCP is attached at all — not a dump of every discovered tool into whatever
/// log formats this executor.
impl std::fmt::Debug for ServerSessionMutationExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerSessionMutationExecutor")
            .field("environment", &self.environment)
            .field("requests", &self.requests)
            .field("runs", &self.runs)
            .field("mcp", &self.mcp.is_some())
            .finish()
    }
}

impl SessionMutationExecutor for ServerSessionMutationExecutor {
    fn prompt(
        &self,
        request: SessionPromptExecution,
        guard: SessionRunGuard,
        events: TurnEventSender,
    ) -> SessionMutationFuture {
        let executor = self.clone();
        Box::pin(async move {
            let mut host = executor
                .open(
                    request.session_id,
                    request.directory,
                    request.agent,
                    request.model,
                )
                .await?;
            let events = host.with_event_hooks(events);
            let outcome = async {
                host.drive_with_message_id_and_guard(
                    &request.prompt,
                    Some(&request.message_id),
                    guard,
                    events.clone(),
                )
                .await?;
                while host
                    .continue_goal_if_idle(zuno_goal::QueuedUserInput::Absent, events.clone())
                    .await?
                {}
                Ok(())
            }
            .await;
            host.shutdown().await;
            outcome
        })
    }

    fn compact(
        &self,
        request: SessionCompactExecution,
        guard: SessionRunGuard,
        events: TurnEventSender,
    ) -> SessionMutationFuture {
        let executor = self.clone();
        Box::pin(async move {
            let mut host = executor
                .open(
                    request.session_id,
                    request.directory,
                    request.agent,
                    request.model,
                )
                .await?;
            let events = host.with_event_hooks(events);
            let outcome = host.compact(request.automatic).await;
            drop(guard);
            let outcome = match outcome {
                Ok(()) => {
                    while host
                        .continue_goal_if_idle(zuno_goal::QueuedUserInput::Absent, events.clone())
                        .await?
                    {}
                    Ok(())
                }
                Err(error) => Err(error),
            };
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
        let project = zuno_paths::project::resolve_project(&directory_path);
        let worktree = project
            .vcs
            .as_ref()
            .map_or(directory_path.as_path(), |_| project.directory.as_path());
        let layout = zuno_paths::Layout::resolve(env);
        let plugin_config =
            zuno_config::discovery::discover_with(&zuno_config::discovery::DiscoveryOptions::new(
                &directory_path,
                Some(worktree),
                env.clone(),
            ))
            .map_err(|error| error.report())?;
        let compat = CompatV1State::new();
        let pool = Arc::new(zuno_db::Pool::open_default().map_err(|error| error.to_string())?);
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
        let services =
            ServerServices::new(DEFAULT_EVENT_SUBSCRIBER_CAPACITY).with_requests(requests.clone());
        // Connected once for the server's lifetime, not per request: every host this
        // executor builds reads the same merged catalog. See `super::mcp_runtime`.
        let mcp = super::mcp_runtime::McpRuntime::from_config(&plugin_config, worktree);
        if let Some(mcp) = mcp.as_ref() {
            for note in mcp.connect().await {
                println!("{note}");
            }
        }
        let mutations = Arc::new(ServerSessionMutationExecutor::new(
            requests,
            services.runs.clone(),
            mcp.as_ref().map(super::mcp_runtime::McpRuntime::catalog),
        ));
        let services = services.with_mutations(mutations);
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
            super::plugin_runtime::JsPluginPolicy::resolve(&plugin_config, env),
            super::plugin_runtime::PluginRuntimeTarget::server_with_stdio(
                "serve",
                plugin_server_url,
            ),
        )
        .await
        .map(Arc::new);
        if let Some(plugins) = &plugins {
            compat.set_provider_oauth_backend(
                Arc::clone(plugins) as Arc<dyn zuno_server::ProviderOAuthBackend>
            );
        }
        println!("{}", server_readiness_message(server.local_addr()));
        std::io::stdout()
            .flush()
            .map_err(|error| error.to_string())?;
        let result = server.serve().await.map_err(|error| error.to_string());
        if let Some(plugins) = plugins {
            plugins.shutdown().await;
        }
        if let Some(mcp) = mcp {
            mcp.shutdown().await;
        }
        result
    })
}

fn server_readiness_message(address: std::net::SocketAddr) -> String {
    format!("Zuno server listening on http://{address}")
}

#[cfg(test)]
mod tests {
    use super::server_readiness_message;

    #[test]
    fn readiness_message_presents_zunos_identity() {
        let address = "127.0.0.1:4096".parse().expect("valid fixture address");
        assert_eq!(
            server_readiness_message(address),
            "Zuno server listening on http://127.0.0.1:4096"
        );
    }
}
