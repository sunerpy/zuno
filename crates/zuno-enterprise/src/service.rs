//! Real role handlers composed from existing application/runtime components.

use crate::{
    Error,
    config::{
        self, ControlConfig, GatewayConfig, ServiceConfig, ServiceRole, StateClientConfig,
        WorkerConfig,
    },
    invalid,
};
use std::{collections::BTreeMap, path::Path, sync::Arc, time::Duration};
use zuno_engine::{
    advance::AdvanceOutcome,
    driver::{AgentDriver, AgentDriverComponent, DefaultAgentDriver},
    interrupt::InterruptSignal,
};
use zuno_identity::{
    gateway::{GatewayServiceAuthority, GatewayTicketAuthority},
    worker::{JobGrantAuthority, WorkerAuthority},
};
use zuno_postgres::PostgresBackend;
use zuno_runtime::{HarnessProfile, HarnessRuntime, ProfileBundle};
use zuno_server::{
    enterprise_application::{ApplicationWorkspace, EnterpriseApplication},
    enterprise_gateway::GatewayControlService,
    enterprise_state::WorkerStateService,
    gateway_configuration::{ConfiguredGateways, GatewayDeployment},
    gateway_execution::GatewayExecutionService,
};
use zuno_types::identity::{JobId, WorkerInstanceId};
use zuno_worker::{
    FileAccessTokenSource, WorkerClient,
    gateway::{GatewayClient, GatewayStateClient},
    runtime::{WorkerError, WorkerObserver, WorkerRuntime, WorkerSettings},
};

pub async fn run(config: ServiceConfig, shutdown: InterruptSignal) -> Result<(), Error> {
    if !cfg!(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    )) {
        return Err(invalid("enterprise services support Linux amd64 and arm64"));
    }
    if !config.state_directory.is_absolute() {
        return Err(invalid(
            "stateDirectory must be an explicit absolute preview directory",
        ));
    }
    let mut directory = tokio::fs::DirBuilder::new();
    directory.recursive(true);
    #[cfg(unix)]
    directory.mode(0o700);
    directory.create(&config.state_directory).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if tokio::fs::metadata(&config.state_directory)
            .await?
            .permissions()
            .mode()
            & 0o077
            != 0
        {
            return Err(invalid(
                "stateDirectory must be private to the service account",
            ));
        }
    }
    let _logs = zuno_observability::init(
        zuno_observability::LogConfig::from_env(config.state_directory.join("logs"))
            .with_print_logs(true),
    )
    .map_err(|_| invalid("could not initialize enterprise operational logging"))?;
    match config.service {
        ServiceRole::Worker(options) => worker(options, shutdown).await,
        ServiceRole::Gateway(options) => gateway(options, &config.state_directory, shutdown).await,
        ServiceRole::ControlPlane(options) => control(*options, shutdown).await,
        ServiceRole::Migrate(options) => {
            let pool = options.database.options().await?.connect().await?;
            zuno_postgres::migrate(&pool, &options.runtime_role).await?;
            if let Some(bootstrap) = options.bootstrap {
                zuno_postgres::bootstrap_organization(
                    &pool,
                    &bootstrap.policy,
                    &bootstrap.administrator,
                )
                .await?;
            }
            pool.close().await;
            Ok(())
        }
        ServiceRole::Identity(options) => {
            let identity = crate::identity::verifier(&options.verifier)
                .await?
                .verify(config::secret(&options.access_token_file).await?.expose())
                .await?;
            println!(
                "{}",
                serde_json::json!({
                    "issuer":identity.issuer(),"tenantId":identity.tenant_id(),
                    "principalId":identity.principal_id(),"clientId":identity.client_id(),
                    "oauthClientId":identity.oauth_client_id(),"kind":format!("{:?}",identity.kind()),
                })
            );
            Ok(())
        }
    }
}

async fn certificate(
    path: Option<&std::path::PathBuf>,
) -> Result<Option<reqwest::Certificate>, Error> {
    match path {
        Some(path) => Ok(Some(
            reqwest::Certificate::from_pem(&config::read_file(path, 65536).await?)
                .map_err(|_| invalid("invalid service CA certificate"))?,
        )),
        None => Ok(None),
    }
}
pub async fn state_client(options: &StateClientConfig) -> Result<WorkerClient, Error> {
    WorkerClient::new(
        config::https_endpoint(&options.endpoint)?,
        Arc::new(FileAccessTokenSource::new(
            options.access_token_file.clone(),
        )),
        certificate(options.root_certificate.as_ref()).await?,
    )
    .map_err(|_| invalid("invalid state API client configuration"))
}
struct Observer;
impl WorkerObserver for Observer {
    fn event(&self, _job: &JobId, _event: zuno_engine::r#loop::TurnEvent) {
        // The public durable projection is independent of this transient stream.
        // No provider thought/signature block is copied to operational logs.
    }
    fn settled(&self, job: &JobId, result: &Result<AdvanceOutcome, WorkerError>) {
        match result {
            Ok(outcome) => {
                tracing::info!(job_id=%job,outcome=?outcome,"bounded Worker advance settled")
            }
            Err(error) => tracing::warn!(job_id=%job,error=%error,"bounded Worker advance stopped"),
        }
    }
}
async fn worker(options: WorkerConfig, shutdown: InterruptSignal) -> Result<(), Error> {
    if options.instance_prefix.is_empty()
        || options.instance_prefix.len() > 80
        || !options
            .instance_prefix
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
    {
        return Err(invalid("invalid Worker instance prefix"));
    }
    let identity = WorkerInstanceId::new(format!(
        "{}-{}",
        options.instance_prefix,
        uuid::Uuid::new_v4().simple()
    ))
    .map_err(|_| invalid("invalid Worker identity"))?;
    let state = state_client(&options.state).await?;
    let gateway = Arc::new(
        GatewayClient::new(certificate(options.state.root_certificate.as_ref()).await?)
            .map_err(|_| invalid("invalid gateway TLS client"))?,
    );
    let runtime = HarnessRuntime::new("enterprise-worker");
    runtime
        .activate_profile(
            HarnessProfile::new("enterprise-worker").with_bundle(
                ProfileBundle::new("kernel")
                    .with_component(AgentDriverComponent::new(Arc::new(DefaultAgentDriver))),
            ),
        )
        .await?;
    let result = async {
        let driver = runtime
            .service::<dyn AgentDriver>()
            .ok_or_else(|| invalid("the profile did not install its AgentDriver"))?;
        let factory = Arc::new(
            crate::profile::ConfiguredWorkerFactory::new(
                config::definitions(&options.definitions).await?,
                &options.credentials,
                state.clone(),
                gateway,
                driver,
            )
            .await?
            .with_live_interval(options.live_millis)?,
        );
        let worker = WorkerRuntime::new(
            state,
            identity,
            factory,
            Arc::new(Observer),
            WorkerSettings {
                slots: options.slots,
                poll_interval: Duration::from_millis(options.poll_millis),
                renew_interval: Duration::from_millis(options.renew_millis),
                drain_timeout: Duration::from_secs(options.drain_seconds),
            },
        )?;
        worker.run(shutdown).await?;
        Ok::<_, Error>(())
    }
    .await;
    let stopped = runtime.shutdown().await;
    result?;
    stopped?;
    Ok(())
}

async fn gateway(
    options: GatewayConfig,
    state_directory: &Path,
    shutdown: InterruptSignal,
) -> Result<(), Error> {
    if !(100..=30000).contains(&options.delivery_millis) {
        return Err(invalid("gateway delivery interval must be 100–30000ms"));
    }
    let state = GatewayStateClient::new(
        config::https_endpoint(&options.state.endpoint)?,
        Arc::new(FileAccessTokenSource::new(
            options.state.access_token_file.clone(),
        )),
        certificate(options.state.root_certificate.as_ref()).await?,
    )
    .map_err(|_| invalid("invalid gateway state client"))?;
    let gateway = GatewayExecutionService::connect(
        options.id,
        &options.docker_socket,
        &state_directory.join("gateway.sqlite"),
        state,
    )
    .await?
    .with_merge_parallelism(options.merge_parallelism)?;
    let delivery = gateway.clone();
    let stopped = shutdown.clone();
    let supervisor = tokio::spawn(async move {
        let mut delay = Duration::from_millis(options.delivery_millis);
        loop {
            tokio::select! {
                biased;
                _=stopped.notified()=>return,
                _=tokio::time::sleep(delay)=>{},
            }
            match delivery.deliver_completions(128).await {
                Ok(_) => delay = Duration::from_millis(options.delivery_millis),
                Err(error) => {
                    tracing::warn!(error=%error,"gateway receipt delivery deferred");
                    delay = (delay * 2).min(Duration::from_secs(30));
                }
            }
        }
    });
    let mut supervisor = supervisor;
    let server = crate::http::serve(&options.tls, gateway.clone().router(), shutdown.clone());
    tokio::pin!(server);
    let (result, supervisor_finished) = tokio::select! {
        result = &mut server => (result,false),
        finished = &mut supervisor => {
            let requested = shutdown.is_set();
            shutdown.fire();
            let result = server.await;
            let result=if finished.is_err() || !requested {
                Err(invalid("gateway receipt supervisor stopped unexpectedly"))
            }else {result};
            (result,true)
        }
    };
    shutdown.fire();
    if !supervisor_finished
        && tokio::time::timeout(Duration::from_secs(31), &mut supervisor)
            .await
            .is_err()
    {
        supervisor.abort();
        let _ = supervisor.await;
    }
    gateway.drain_merges(Duration::from_secs(30)).await;
    result
}

async fn control(options: ControlConfig, shutdown: InterruptSignal) -> Result<(), Error> {
    if options.web_assets_directory.is_some() && options.browser.is_none() {
        return Err(invalid("Web assets require browser OIDC configuration"));
    }
    let lease = options.lease()?;
    let backend = PostgresBackend::connect(options.database.options().await?).await?;
    let users = crate::identity::verifier(&options.user_identity).await?;
    let services = crate::identity::verifier(&options.service_identity).await?;
    let workers = Arc::new(
        WorkerAuthority::new(services.clone(), options.workers)
            .map_err(|_| invalid("invalid Worker service allowlist"))?,
    );
    let mut allowed = BTreeMap::new();
    for entry in options.gateways {
        if allowed.insert(entry.subject, entry.gateway_id).is_some() {
            return Err(invalid("duplicate gateway service subject"));
        }
    }
    let gateways = Arc::new(
        GatewayServiceAuthority::new(services, allowed)
            .map_err(|_| invalid("invalid gateway service allowlist"))?,
    );
    let grants = Arc::new(
        JobGrantAuthority::new(
            options.job_keys.active.clone(),
            options.job_keys.load().await?,
        )
        .map_err(|_| invalid("invalid Job signing keys"))?,
    );
    let tickets = Arc::new(
        GatewayTicketAuthority::new(
            options.gateway_keys.active.clone(),
            options.gateway_keys.load().await?,
            30000,
        )
        .map_err(|_| invalid("invalid gateway signing keys"))?,
    );
    let definitions = config::definitions(&options.definitions).await?;
    let children = Arc::new(crate::children::ConfiguredChildren::new(&definitions)?);
    let workflows = Arc::new(crate::workflows::ConfiguredWorkflows::new(
        &definitions,
        &children,
    )?);
    let councils = Arc::new(crate::councils::ConfiguredCouncils::new(
        &definitions,
        &children,
    )?);
    let deployments = definitions
        .iter()
        .filter_map(|definition| {
            definition
                .environment
                .as_ref()
                .map(|environment| (definition, environment))
        })
        .map(|(definition, environment)| {
            Ok(GatewayDeployment {
                tenant: options.tenant_id.clone(),
                configuration: definition.reference(),
                gateway_id: environment.gateway_id.clone(),
                endpoint: config::https_endpoint(&environment.endpoint)?,
                image: environment.image.clone(),
                memory_bytes: environment.memory_bytes,
                pids_limit: environment.pids_limit,
                cpu_millis: environment.cpu_millis,
            })
        })
        .collect::<Result<Vec<_>, Error>>()?;
    let has_environments = !deployments.is_empty();
    let assignments = Arc::new(ConfiguredGateways::new(deployments)?);
    let mut active = Vec::new();
    for selected in &options.active_definitions {
        let definition = definitions
            .iter()
            .find(|definition| {
                definition.id == selected.id && definition.version == selected.version
            })
            .ok_or_else(|| invalid("an active configuration is not installed"))?;
        active.push(ApplicationWorkspace {
            id: definition.workspace.id.clone(),
            title: definition.workspace.title.clone(),
            configuration: definition.reference(),
            selection: definition.selection(),
        });
    }
    let memory =
        zuno_postgres::PostgresMemoryBackend::new(backend.clone(), options.memory.limits())
            .map_err(|_| invalid("invalid Memory service configuration"))?;
    let mut application =
        EnterpriseApplication::new(backend.clone(), options.tenant_id.clone(), active)?
            .with_memory(memory.clone());
    if has_environments {
        application = application.with_workspace_gateway(Arc::new(
            zuno_server::workspace_gateway::GatewayWorkspaceClient::new(
                backend.clone(),
                tickets.clone(),
                assignments.clone(),
                certificate(options.gateway_root_certificate.as_ref()).await?,
            )?,
        ));
    }
    let mut worker_state = WorkerStateService::new(
        backend.clone(),
        workers.clone(),
        grants.clone(),
        options.tenant_id.clone(),
        lease,
    )
    .with_memory(memory)
    .with_memory_configurations(
        definitions
            .iter()
            .filter(|definition| definition.agent.mode == config::AgentExecutionMode::Agent)
            .map(|definition| definition.reference())
            .collect(),
    )?;
    if !children.is_empty() {
        worker_state = worker_state.with_children(children);
    }
    if !workflows.is_empty() {
        worker_state = worker_state.with_workflows(workflows);
    }
    if !councils.is_empty() {
        worker_state = worker_state.with_councils(councils);
    }
    let mut routes = application
        .clone()
        .api_router(users.clone())
        .merge(worker_state.router());
    if has_environments {
        routes = routes.merge(
            GatewayControlService::new(
                backend.clone(),
                options.tenant_id.clone(),
                workers,
                grants,
                gateways,
                tickets,
                assignments,
            )
            .router(),
        );
    }
    if let Some(browser) = &options.browser {
        let browser = crate::identity::browser(browser, &backend, options.tenant_id, users).await?;
        routes = routes
            .merge(application.browser_router(&browser))
            .merge(browser.router());
    }
    if let Some(directory) = &options.web_assets_directory {
        routes = routes.merge(crate::web::WebAssets::load(directory).await?.router());
    }
    crate::http::serve(&options.tls, routes, shutdown).await
}
