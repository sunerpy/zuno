use std::io::Write as _;
use std::sync::Arc;

use oc_engine::interrupt::InterruptSignal;
use oc_engine::r#loop::TurnEventSender;
use oc_server::api::{self, ApiState};
use oc_server::{
    AuthConfig, CompatV1State, DEFAULT_EVENT_SUBSCRIBER_CAPACITY, EventService, ServerBuilder,
    ServerConfig, ServerServices, SessionCompactExecution, SessionMutationExecutor,
    SessionMutationFuture, SessionPromptExecution, compat_v1_router, events_router,
};

use super::tool_runtime::HeadlessApproval;
use super::turn::{SessionChoice, TurnHost, TurnOptions, TurnPlan};
use crate::command::ServeArgs;
use crate::environment::StartupEnvironment;

#[derive(Clone, Debug)]
struct ServerSessionMutationExecutor {
    environment: StartupEnvironment,
}

impl ServerSessionMutationExecutor {
    fn new() -> Self {
        Self {
            environment: StartupEnvironment::resolve(
                &oc_paths::Env::from_process(),
                &crate::command::GlobalOptions::default(),
            ),
        }
    }

    async fn open(
        environment: StartupEnvironment,
        session_id: String,
        directory: std::path::PathBuf,
        agent: Option<String>,
        model: Option<oc_server::SessionModelSelection>,
        interrupt: InterruptSignal,
    ) -> Result<TurnHost, String> {
        let options = TurnOptions {
            directory: Some(directory),
            model: model.map(|model| format!("{}/{}", model.provider_id, model.model_id)),
            agent,
            session: SessionChoice::Existing(session_id),
            title: None,
        };
        let plan = TurnPlan::resolve(&options, &environment).await?;
        TurnHost::open_with_interrupt(plan, &environment, Arc::new(HeadlessApproval), interrupt)
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
        Box::pin(async move {
            let mut host = Self::open(
                environment,
                request.session_id,
                request.directory,
                request.agent,
                request.model,
                interrupt,
            )
            .await?;
            host.drive_with_message_id(&request.prompt, Some(&request.message_id), events)
                .await
        })
    }

    fn compact(
        &self,
        request: SessionCompactExecution,
        interrupt: InterruptSignal,
    ) -> SessionMutationFuture {
        let environment = self.environment.clone();
        Box::pin(async move {
            let mut host = Self::open(
                environment,
                request.session_id,
                request.directory,
                request.agent,
                request.model,
                interrupt,
            )
            .await?;
            host.compact().await
        })
    }
}

pub(super) fn execute(args: &ServeArgs) -> Result<(), String> {
    if args.mdns {
        return Err("--mdns is not supported by the Rust server runtime yet".to_owned());
    }
    if args.mdns_domain != "opencode.local" {
        return Err("--mdns-domain requires --mdns, which is not supported yet".to_owned());
    }
    if !args.cors.is_empty() {
        return Err("--cors is not supported by the Rust server runtime yet".to_owned());
    }

    let directory = std::env::current_dir()
        .map_err(|error| error.to_string())?
        .to_string_lossy()
        .into_owned();
    let auth = AuthConfig::from_env();
    if !auth.required() {
        println!("Warning: OPENCODE_SERVER_PASSWORD is not set; server is unsecured.");
    }

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    runtime.block_on(async {
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
        let services = ServerServices::new(DEFAULT_EVENT_SUBSCRIBER_CAPACITY)
            .with_mutations(Arc::new(ServerSessionMutationExecutor::new()));
        let server = ServerBuilder::new(config)
            .with_services(services)
            .with_routes(
                api::router(state)
                    .merge(events_router(events))
                    .merge(compat_v1_router(CompatV1State::new())),
            )
            .bind()
            .await
            .map_err(|error| error.to_string())?;
        println!(
            "opencode server listening on http://{}",
            server.local_addr()
        );
        std::io::stdout()
            .flush()
            .map_err(|error| error.to_string())?;
        server.serve().await.map_err(|error| error.to_string())
    })
}
