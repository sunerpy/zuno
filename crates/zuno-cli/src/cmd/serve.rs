use std::io::Write as _;
use std::sync::Arc;

use async_trait::async_trait;
use uuid::Uuid;
use zuno_engine::r#loop::{TurnEvent, TurnEventSender};
use zuno_engine::status::{SessionRunGuard, SessionRunRegistry};
use zuno_error::ToolError;
use zuno_permission::ReplyKind;
use zuno_server::api::{self, ApiState};
use zuno_server::{
    AuthConfig, DEFAULT_EVENT_SUBSCRIBER_CAPACITY, EventFanout, EventService, NewEvent,
    PermissionRequest, QuestionDecision, QuestionRequest, QuestionToolCall, RequestBroker,
    ServerBuilder, ServerConfig, ServerServices, SessionCompactExecution, SessionMutationExecutor,
    SessionMutationFuture, SessionPromptExecution, events_router,
};
use zuno_tool::{PermissionAsk, PermissionAsker, PermissionOrigin};
use zuno_tools::question::{QuestionAsker, QuestionOutcome};

use super::child_turn::DetachedTurnObserver;
use super::turn::{SessionChoice, TurnHost, TurnHostRuntimeDependencies, TurnOptions, TurnPlan};
use crate::command::ServeArgs;
use crate::environment::StartupEnvironment;

#[derive(Clone)]
struct ServerSessionMutationExecutor {
    environment: StartupEnvironment,
    requests: RequestBroker,
    runs: SessionRunRegistry,
    /// Serializes host acquisition with extension transition reservation.
    ///
    /// Turns themselves remain concurrent. Only the short composition boundary is
    /// serialized so a request cannot acquire the old revision after a candidate has
    /// reserved it and before that candidate commits.
    composition_gate: Arc<tokio::sync::Mutex<()>>,
    detached_observer: Arc<ServerDetachedTurnObserver>,
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
        environment: StartupEnvironment,
        requests: RequestBroker,
        runs: SessionRunRegistry,
        mcp: Option<zuno_mcp::Catalog>,
        events: EventService,
        fanout: EventFanout<TurnEvent>,
    ) -> Self {
        Self {
            environment,
            requests,
            runs,
            composition_gate: Arc::new(tokio::sync::Mutex::new(())),
            detached_observer: Arc::new(ServerDetachedTurnObserver { events, fanout }),
            mcp,
        }
    }

    async fn open_active(&self, spec: &ServerHostSpec) -> Result<TurnHost, String> {
        let _composition = self.composition_gate.lock().await;
        let plan = TurnPlan::resolve(
            &spec.options(super::turn::ExtensionComposition::Active),
            &self.environment,
        )
        .await?;
        self.open_plan(plan).await
    }

    async fn open_plan(&self, plan: TurnPlan) -> Result<TurnHost, String> {
        let approval: Arc<dyn PermissionAsker> = Arc::new(ServerPermissionAsker {
            requests: self.requests.clone(),
        });
        let question: Arc<dyn QuestionAsker> = Arc::new(ServerQuestionAsker {
            requests: self.requests.clone(),
        });
        let mut host = TurnHost::open_with_runtime_mcp_and_observers(
            plan,
            &self.environment,
            TurnHostRuntimeDependencies {
                approval,
                question: Some(question),
                runs: self.runs.clone(),
                mcp: self.mcp.clone(),
                child_observer: None,
                detached_observer: Some(
                    Arc::clone(&self.detached_observer) as Arc<dyn DetachedTurnObserver>
                ),
            },
        )
        .await?;
        if let Err(error) = host.activate_extension_composition() {
            let shutdown = host.shutdown().await;
            return Err(match shutdown {
                Ok(()) => error,
                Err(shutdown) => {
                    format!("{error}; candidate host shutdown also failed: {shutdown}")
                }
            });
        }
        host.activate_background_notifications(&tokio::runtime::Handle::current());
        Ok(host)
    }

    fn final_work_state(
        &self,
        host: &TurnHost,
        session_id: &str,
    ) -> Option<zuno_types::WorkStateProjection> {
        match host.work_state() {
            Ok(work) => Some(work),
            Err(error) => {
                tracing::debug!(
                    session_id,
                    %error,
                    "failed to read final server turn work state for projection"
                );
                None
            }
        }
    }

    /// Publish a staged extension mutation once all request hosts on the old revision
    /// have stopped. A live peer defers the transition; its own shutdown will retry.
    async fn reconcile_extensions(
        &self,
        spec: &ServerHostSpec,
        scope: &zuno_extension::Scope,
    ) -> Result<(), String> {
        let _composition = self.composition_gate.lock().await;
        let prepared =
            match reserve_pending_extension_transition(self.environment.extensions(), scope)? {
                PendingExtensionReservation::None | PendingExtensionReservation::Deferred => {
                    return Ok(());
                }
                PendingExtensionReservation::Reserved(prepared) => prepared,
            };
        let mut plan = match TurnPlan::resolve(
            &spec.options(super::turn::ExtensionComposition::Desired),
            &self.environment,
        )
        .await
        {
            Ok(plan) => plan,
            Err(error) => {
                return match prepared.abort() {
                    Ok(()) => Err(error),
                    Err(abort) => Err(format!(
                        "{error}; prepared extension transition abort also failed: {abort}"
                    )),
                };
            }
        };
        plan.use_prepared_extension_transition(prepared)?;
        let mut candidate = self.open_plan(plan).await?;
        candidate.shutdown().await
    }
}

#[derive(Clone)]
struct ServerHostSpec {
    session_id: String,
    directory: std::path::PathBuf,
    agent: Option<String>,
    model: Option<zuno_server::SessionModelSelection>,
}

impl ServerHostSpec {
    fn options(&self, extension_composition: super::turn::ExtensionComposition) -> TurnOptions {
        TurnOptions {
            directory: Some(self.directory.clone()),
            model: self
                .model
                .as_ref()
                .map(|model| format!("{}/{}", model.provider_id, model.model_id)),
            agent: self.agent.clone(),
            preset: None,
            session: SessionChoice::Existing(self.session_id.clone()),
            title: None,
            effort: None,
            variant: None,
            thinking: false,
            tool_authority: None,
            extension_composition,
        }
    }
}

enum PendingExtensionReservation {
    None,
    Deferred,
    Reserved(zuno_extension::PreparedTransition),
}

fn reserve_pending_extension_transition(
    registry: &Arc<zuno_extension::ExtensionRegistry>,
    scope: &zuno_extension::Scope,
) -> Result<PendingExtensionReservation, String> {
    let Some(transaction) = registry.pending_transaction(scope) else {
        return Ok(PendingExtensionReservation::None);
    };
    match registry.begin_transition(&transaction) {
        Ok(prepared) => Ok(PendingExtensionReservation::Reserved(prepared)),
        Err(
            zuno_extension::RegistryError::ActiveConsumers { .. }
            | zuno_extension::RegistryError::TransitionReserved,
        ) => Ok(PendingExtensionReservation::Deferred),
        Err(zuno_extension::RegistryError::TransactionMismatch { .. })
            if registry.pending_transaction(scope).as_ref() != Some(&transaction) =>
        {
            Ok(PendingExtensionReservation::Deferred)
        }
        Err(error) => Err(error.to_string()),
    }
}

#[derive(Debug)]
struct ServerPermissionAsker {
    requests: RequestBroker,
}

#[async_trait]
impl PermissionAsker for ServerPermissionAsker {
    async fn ask(
        &self,
        origin: PermissionOrigin<'_>,
        tool: &str,
        ask: PermissionAsk,
    ) -> Result<(), ToolError> {
        let request = PermissionRequest {
            id: format!("per_{}", Uuid::new_v4().simple()),
            session_id: origin.session_id().to_owned(),
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
    ) -> Result<QuestionOutcome, ToolError> {
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
        Ok(match self.requests.ask_question(request).await {
            QuestionDecision::Answered(answers) => QuestionOutcome::Answered(answers),
            QuestionDecision::Cancelled => QuestionOutcome::Cancelled,
            QuestionDecision::Expired => QuestionOutcome::Expired,
            QuestionDecision::Failed => QuestionOutcome::Failed,
        })
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
            .field("detached_observer", &"configured")
            .field("mcp", &self.mcp.is_some())
            .finish()
    }
}

struct ServerDetachedTurnObserver {
    events: EventService,
    fanout: EventFanout<TurnEvent>,
}

#[async_trait]
impl DetachedTurnObserver for ServerDetachedTurnObserver {
    async fn event(&self, session_id: &str, event: &TurnEvent) {
        self.events
            .forward_engine_event(session_id, &self.fanout, event.clone())
            .await;
    }

    async fn work_state(&self, session_id: &str, work: &zuno_types::WorkStateProjection) {
        let properties = match serde_json::to_value(&work.learning) {
            Ok(serde_json::Value::Object(properties)) => properties,
            Ok(_) => {
                tracing::debug!(
                    session_id,
                    "learning work-state projection did not serialize as an object"
                );
                return;
            }
            Err(error) => {
                tracing::debug!(
                    session_id,
                    %error,
                    "failed to serialize final server learning projection"
                );
                return;
            }
        };
        let event = match NewEvent::new("learning.state.changed", properties) {
            Ok(event) => event,
            Err(error) => {
                tracing::debug!(
                    session_id,
                    %error,
                    "failed to construct durable server learning projection event"
                );
                return;
            }
        };
        if let Err(error) = self.events.publish(session_id, event).await {
            tracing::debug!(
                session_id,
                %error,
                "failed to publish durable server learning projection event"
            );
        }
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
            let spec = ServerHostSpec {
                session_id: request.session_id.clone(),
                directory: request.directory,
                agent: request.agent,
                model: request.model,
            };
            let mut host = executor.open_active(&spec).await?;
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
            let work = executor.final_work_state(&host, &spec.session_id);
            if let Some(work) = work {
                executor
                    .detached_observer
                    .work_state(&spec.session_id, &work)
                    .await;
            }
            let extension_scope = host.extension_scope().clone();
            let shutdown = host.shutdown().await;
            let reconciliation = if shutdown.is_ok() {
                executor.reconcile_extensions(&spec, &extension_scope).await
            } else {
                Ok(())
            };
            finish_server_mutation(outcome, shutdown, reconciliation)
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
            let spec = ServerHostSpec {
                session_id: request.session_id,
                directory: request.directory,
                agent: request.agent,
                model: request.model,
            };
            let mut host = executor.open_active(&spec).await?;
            let outcome = async {
                host.compact_with_guard(request.automatic, guard, events.clone())
                    .await?;
                while host
                    .continue_goal_if_idle(zuno_goal::QueuedUserInput::Absent, events.clone())
                    .await?
                {}
                Ok(())
            }
            .await;
            let work = executor.final_work_state(&host, &spec.session_id);
            if let Some(work) = work {
                executor
                    .detached_observer
                    .work_state(&spec.session_id, &work)
                    .await;
            }
            let extension_scope = host.extension_scope().clone();
            let shutdown = host.shutdown().await;
            let reconciliation = if shutdown.is_ok() {
                executor.reconcile_extensions(&spec, &extension_scope).await
            } else {
                Ok(())
            };
            finish_server_mutation(outcome, shutdown, reconciliation)
        })
    }
}

fn finish_server_mutation(
    outcome: Result<(), String>,
    shutdown: Result<(), String>,
    reconciliation: Result<(), String>,
) -> Result<(), String> {
    let mut failures = outcome.err().into_iter().collect::<Vec<_>>();
    failures.extend(
        shutdown
            .err()
            .map(|error| format!("turn host shutdown failed: {error}")),
    );
    failures.extend(
        reconciliation
            .err()
            .map(|error| format!("extension reconciliation failed: {error}")),
    );
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; "))
    }
}

pub(super) fn execute(args: &ServeArgs, environment: &StartupEnvironment) -> Result<(), String> {
    if args.mdns {
        return Err("--mdns is not supported by the Rust server runtime yet".to_owned());
    }
    if args.mdns_domain != "zuno.local" {
        return Err("--mdns-domain requires --mdns, which is not supported yet".to_owned());
    }
    if !args.cors.is_empty() {
        return Err("--cors is not supported by the Rust server runtime yet".to_owned());
    }

    let directory_path = std::env::current_dir().map_err(|error| error.to_string())?;
    let directory = directory_path.to_string_lossy().into_owned();
    let auth = AuthConfig::from_env();
    if !auth.required() {
        println!("Warning: ZUNO_SERVER_PASSWORD is not set; server is unsecured.");
    }

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    runtime.block_on(async {
        let env = environment.resolved();
        let project = zuno_paths::project::resolve_project(&directory_path);
        let mcp_workspace = project
            .vcs
            .as_ref()
            .map_or(directory_path.as_path(), |_| project.directory.as_path());
        let harness_config = zuno_config::discovery::discover_with(
            &zuno_config::discovery::DiscoveryOptions::for_project(
                &directory_path,
                &project,
                env.clone(),
            ),
        )
        .map_err(|error| error.report())?;
        zuno_pty::shells::preferred(harness_config.shell.as_deref())
            .map_err(|error| format!("invalid shell configuration: {error}"))?;
        let pool = Arc::new(zuno_db::Pool::open_default().map_err(|error| error.to_string())?);
        let events = EventService::new(Arc::clone(&pool), DEFAULT_EVENT_SUBSCRIBER_CAPACITY);
        let server_config = ServerConfig::default()
            .with_hostname(&args.hostname)
            .with_port(args.port)
            .with_auth(auth)
            .with_default_directory(&directory);
        let state = ApiState::open_default(&directory)
            .map_err(|error| error.to_string())?
            .with_configured_shell(harness_config.shell.clone())
            .with_events(events.clone());
        let goals = Arc::new(
            zuno_goal::GoalStore::from_pool(Arc::clone(&pool), zuno_goal::default_spill_dir())
                .map_err(|error| error.to_string())?,
        );
        let requests = RequestBroker::with_events(events.clone())
            .with_store(zuno_db::human_request::HumanRequestStore::new(Arc::clone(
                &pool,
            )))
            .with_goal_store(goals);
        let services =
            ServerServices::new(DEFAULT_EVENT_SUBSCRIBER_CAPACITY).with_requests(requests.clone());
        // Connected once for the server's lifetime, not per request: every host this
        // executor builds reads the same merged catalog. See `super::mcp_runtime`.
        let mcp = super::mcp_runtime::McpRuntime::from_config(&harness_config, mcp_workspace);
        if let Some(mcp) = mcp.as_ref() {
            for note in mcp.connect().await {
                println!("{note}");
            }
        }
        let mutations = Arc::new(ServerSessionMutationExecutor::new(
            environment.clone(),
            requests,
            services.runs.clone(),
            mcp.as_ref().map(super::mcp_runtime::McpRuntime::catalog),
            events.clone(),
            services.events.clone(),
        ));
        let services = services.with_mutations(mutations);
        let server = ServerBuilder::new(server_config)
            .with_services(services)
            .with_routes(api::router(state.clone()).merge(events_router(events)))
            .bind()
            .await
            .map_err(|error| error.to_string())?;
        println!("{}", server_readiness_message(server.local_addr()));
        std::io::stdout()
            .flush()
            .map_err(|error| error.to_string())?;
        let result = server.serve().await.map_err(|error| error.to_string());
        environment.cancel_background_jobs();
        environment.wait_background_jobs().await;
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
    use std::path::Path;
    use std::sync::Arc;

    use serde_json::json;

    use super::{
        PendingExtensionReservation, reserve_pending_extension_transition, server_readiness_message,
    };

    #[test]
    fn readiness_message_presents_zunos_identity() {
        let address = "127.0.0.1:4096".parse().expect("valid fixture address");
        assert_eq!(
            server_readiness_message(address),
            "Zuno server listening on http://127.0.0.1:4096"
        );
    }

    #[test]
    fn pending_server_extension_waits_for_every_old_host_before_reserving() {
        let registry = Arc::new(zuno_extension::ExtensionRegistry::new());
        let scope = zuno_extension::Scope::new(Path::new("/workspace"));
        let package = serde_json::from_value(json!({
            "apiVersion": zuno_extension::API_VERSION,
            "id": "review",
            "description": "review extension",
            "workflows": {
                "review": {
                    "description": "review",
                    "prompt": "Review the change."
                }
            }
        }))
        .expect("valid extension");
        registry.define(&scope, package).expect("define");
        registry
            .stage_run(&scope, "review", &[])
            .expect("stage activation");
        let old_host = registry
            .acquire_active(&scope, registry.active_revision(&scope))
            .expect("old host lease");

        assert!(matches!(
            reserve_pending_extension_transition(&registry, &scope)
                .expect("live consumers defer instead of failing"),
            PendingExtensionReservation::Deferred
        ));

        drop(old_host);
        let PendingExtensionReservation::Reserved(prepared) =
            reserve_pending_extension_transition(&registry, &scope)
                .expect("last host reserves the transition")
        else {
            panic!("quiescent registry must reserve its candidate");
        };
        assert!(
            registry
                .acquire_active(&scope, registry.active_revision(&scope))
                .is_err(),
            "a late old host must not enter while candidate preparation is reserved"
        );
        prepared.abort().expect("candidate fixture aborts cleanly");
    }
}
