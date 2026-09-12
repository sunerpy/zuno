//! Bounded Worker execution over the shared kernel and authenticated state API.

use async_trait::async_trait;
use std::{num::NonZeroU32, sync::Arc, time::Duration};
use tokio::task::JoinSet;
use zuno_application::runtime::{ConfigurationRef, JobFinish};
use zuno_db::message::{MessageRecord, PartRecord};
use zuno_engine::{
    advance::{AdvanceError, AdvanceOutcome, AdvanceRequest},
    budget::TurnBudgetPolicy,
    driver::AgentDriver,
    interrupt::InterruptSignal,
    r#loop::{
        AgentModelResolver, DynamicContextRefresher, RunTurnRequest, ToolDispatcher, TurnContext,
        TurnEvent, event_channel,
    },
    state::{InputMaterialization, TurnPersistence, TurnStateError, TurnStateScope},
};
use zuno_llm::{cache::DynamicContext, registry::ProviderRegistry};
use zuno_types::identity::{JobId, WorkerInstanceId};

use crate::{WorkerClient, WorkerExecution};

/// Host-created services for one pinned Job definition. No secret/provider
/// object, dispatcher Future or physical database handle crosses the wire.
pub struct WorkerTurnServices {
    pub configuration: ConfigurationRef,
    pub providers: Arc<ProviderRegistry>,
    pub resolver: Arc<dyn AgentModelResolver>,
    pub dispatcher: Arc<dyn ToolDispatcher>,
    pub driver: Arc<dyn AgentDriver>,
    pub budget: Arc<dyn TurnBudgetPolicy>,
    pub dynamic_context: DynamicContext,
    pub dynamic_context_refresher: Option<Arc<dyn DynamicContextRefresher>>,
    pub executor_directory: String,
    pub steps_per_advance: NonZeroU32,
    pub context_limit: Option<u64>,
}

#[async_trait]
pub trait WorkerServiceFactory: Send + Sync {
    /// Immutable definitions installed and validated by this Worker host.
    fn configurations(&self) -> Vec<ConfigurationRef>;
    async fn resolve(&self, execution: &WorkerExecution)
    -> Result<WorkerTurnServices, WorkerError>;
}

/// Live observations are attempt-scoped; durable facts remain in the state API.
/// Implementations enqueue into a bounded local projection buffer without
/// blocking. Network/UI consumers run separately from execution ownership.
pub trait WorkerObserver: Send + Sync {
    fn event(&self, job: &JobId, event: TurnEvent);
    fn settled(&self, job: &JobId, result: &Result<AdvanceOutcome, WorkerError>);
}

#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    #[error("the Worker definition is unavailable or incompatible")]
    Configuration,
    #[error("the Worker lost its execution authority")]
    LeaseLost,
    #[error("the Worker's local grant lifetime expired")]
    LeaseExpired,
    #[error("the Worker could not confirm its final checkpoint")]
    BoundaryUnconfirmed,
    #[error("the Worker state service is unavailable")]
    Unavailable,
    #[error("the shared kernel could not advance")]
    Advance(#[source] AdvanceError),
    #[error("the Worker task failed")]
    Task,
}

fn state_error(error: TurnStateError) -> WorkerError {
    match error {
        TurnStateError::LeaseLost | TurnStateError::Forbidden => WorkerError::LeaseLost,
        TurnStateError::InvalidData | TurnStateError::Conflict => WorkerError::Configuration,
        _ => WorkerError::Unavailable,
    }
}

#[derive(Clone, Copy)]
pub struct WorkerSettings {
    pub slots: u32,
    pub poll_interval: Duration,
    pub renew_interval: Duration,
    pub drain_timeout: Duration,
}
impl WorkerSettings {
    pub fn validate(&self) -> Result<(), WorkerError> {
        if !(1..=64).contains(&self.slots)
            || !(Duration::from_millis(100)..=Duration::from_secs(30)).contains(&self.poll_interval)
            || !(Duration::from_millis(100)..=Duration::from_secs(60))
                .contains(&self.renew_interval)
            || !(Duration::from_secs(1)..=Duration::from_secs(600)).contains(&self.drain_timeout)
        {
            return Err(WorkerError::Configuration);
        }
        Ok(())
    }
}

pub struct WorkerRuntime {
    client: WorkerClient,
    worker: WorkerInstanceId,
    factory: Arc<dyn WorkerServiceFactory>,
    observer: Arc<dyn WorkerObserver>,
    settings: WorkerSettings,
    configurations: Vec<ConfigurationRef>,
}

impl WorkerRuntime {
    pub fn new(
        client: WorkerClient,
        worker: WorkerInstanceId,
        factory: Arc<dyn WorkerServiceFactory>,
        observer: Arc<dyn WorkerObserver>,
        settings: WorkerSettings,
    ) -> Result<Self, WorkerError> {
        settings.validate()?;
        let configurations = factory.configurations();
        crate::validate_configurations(&configurations).map_err(state_error)?;
        Ok(Self {
            client,
            worker,
            factory,
            observer,
            settings,
            configurations,
        })
    }

    /// Draining stops new claims and lets already claimed bounded advances finish.
    /// A hard deadline aborts remaining tasks; their durable in-flight markers
    /// retain conservative recovery instead of manufacturing success.
    pub async fn run(&self, shutdown: InterruptSignal) -> Result<(), WorkerError> {
        let mut running = JoinSet::new();
        let mut poll = tokio::time::interval(self.settings.poll_interval);
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                biased;
                _ = shutdown.notified() => break,
                Some(result) = running.join_next(), if !running.is_empty() => {
                    result.map_err(|_|WorkerError::Task)?;
                }
                _ = poll.tick(), if running.len() < self.settings.slots as usize => {
                    let claim = tokio::select! {
                        biased;
                        _ = shutdown.notified() => break,
                        result = self.client.claim(self.worker.clone(), &self.configurations) => result,
                    };
                    match claim {
                        Ok(Some(execution)) => {
                            let client=self.client.clone();
                            let factory=self.factory.clone();
                            let observer=self.observer.clone();
                            let renew=self.settings.renew_interval;
                            running.spawn(async move {
                                let id=execution.job.id.clone();
                                let result=advance_claimed(&client,&execution,factory.as_ref(),observer.as_ref(),renew).await;
                                observer.settled(&id,&result);
                            });
                        }
                        Ok(None) | Err(TurnStateError::Unavailable) => {}
                        Err(error) => return Err(state_error(error)),
                    }
                }
            }
        }
        match tokio::time::timeout(self.settings.drain_timeout, async {
            while let Some(result) = running.join_next().await {
                result.map_err(|_| WorkerError::Task)?;
            }
            Ok::<_, WorkerError>(())
        })
        .await
        {
            Ok(result) => result?,
            Err(_) => {
                running.abort_all();
                while running.join_next().await.is_some() {}
            }
        }
        Ok(())
    }
}

/// One claim advances only to the next configured checkpoint boundary.
pub async fn advance_claimed(
    client: &WorkerClient,
    execution: &WorkerExecution,
    factory: &dyn WorkerServiceFactory,
    observer: &dyn WorkerObserver,
    renew_interval: Duration,
) -> Result<AdvanceOutcome, WorkerError> {
    if !(Duration::from_millis(100)..=Duration::from_secs(60)).contains(&renew_interval) {
        return Err(WorkerError::Configuration);
    }
    let interrupt = InterruptSignal::new();
    let work = advance_with_services(client, execution, factory, observer, &interrupt);
    tokio::pin!(work);
    loop {
        if execution.boundary_started() {
            return tokio::time::timeout(Duration::from_secs(30), &mut work)
                .await
                .map_err(|_| WorkerError::BoundaryUnconfirmed)?;
        }
        let deadline = execution.deadline().map_err(state_error)?;
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            interrupt.fire();
            return Err(WorkerError::LeaseExpired);
        }
        let delay = renew_interval.min(remaining / 3);
        tokio::select! {
            biased;
            _ = tokio::time::sleep_until(deadline) => {
                if execution.boundary_started() {
                    continue;
                }
                interrupt.fire();
                return Err(WorkerError::LeaseExpired);
            }
            result = &mut work => return result,
            _ = tokio::time::sleep(delay) => {
                let renewed = tokio::select! {
                    biased;
                    _ = tokio::time::sleep_until(deadline) => {
                        if execution.boundary_started() {
                            continue;
                        }
                        interrupt.fire();
                        return Err(WorkerError::LeaseExpired);
                    }
                    result = &mut work => return result,
                    result = client.renew(execution) => result,
                };
                match renewed {
                    Ok(crate::LeaseRenewal::Renewed(_)) => {}
                    Ok(crate::LeaseRenewal::Released) => return work.await,
                    Err(_) => {
                        interrupt.fire();
                        return Err(WorkerError::LeaseLost);
                    }
                }
            }
        }
    }
}

async fn advance_with_services(
    client: &WorkerClient,
    execution: &WorkerExecution,
    factory: &dyn WorkerServiceFactory,
    observer: &dyn WorkerObserver,
    interrupt: &InterruptSignal,
) -> Result<AdvanceOutcome, WorkerError> {
    let services = match factory.resolve(execution).await {
        Ok(services) => services,
        Err(WorkerError::Configuration) => {
            client
                .finish(
                    execution,
                    JobFinish::Failed {
                        code: "worker_configuration".to_owned(),
                    },
                )
                .await
                .map_err(state_error)?;
            return Err(WorkerError::Configuration);
        }
        Err(error) => return Err(error),
    };
    if services.configuration.id != execution.job.configuration.id
        || services.configuration.version != execution.job.configuration.version
        || services.configuration.sha256 != execution.job.configuration.sha256
        || !services.driver.supports_advance()
    {
        client
            .finish(
                execution,
                JobFinish::Failed {
                    code: "worker_configuration".to_owned(),
                },
            )
            .await
            .map_err(state_error)?;
        return Err(WorkerError::Configuration);
    }
    let scope = TurnStateScope {
        owner: execution.job.principal.owner(),
        session_id: execution.job.session_id.to_string(),
    };
    let state = Arc::new(
        client
            .persistence(execution, services.executor_directory)
            .map_err(state_error)?,
    );
    if execution.job.checkpoint.is_none() {
        if execution.input.created_at_ms < 0 {
            return Err(WorkerError::Configuration);
        }
        let input = &execution.input;
        let message = MessageRecord::from_json(serde_json::json!({
            "id":input.id,"sessionID":execution.job.session_id,"role":"user",
            "time":{"created":input.created_at_ms},"agent":input.agent,
            "model":input.model.as_ref().map(|model|serde_json::json!({
                "providerID":model.provider_id,"modelID":model.model_id,
            })),
        }))
        .map_err(|_| WorkerError::Configuration)?;
        let part = PartRecord::from_json(
            serde_json::json!({
                "id":format!("part_{}",input.id),"sessionID":execution.job.session_id,
                "messageID":input.id,"type":"text","text":input.text,
            }),
            input.created_at_ms,
        )
        .map_err(|_| WorkerError::Configuration)?;
        state
            .consume_input(
                &scope,
                InputMaterialization {
                    input_id: Some(input.id.to_string()),
                    turn_id: Some(execution.job.turn_id.to_string()),
                    message,
                    parts: vec![part],
                },
            )
            .await
            .map_err(|error| WorkerError::Advance(AdvanceError::Turn(error)))?;
    }
    let mut run = RunTurnRequest::new(
        execution.job.session_id.to_string(),
        execution.job.turn_id.to_string(),
        services.dynamic_context,
    );
    if let Some(limit) = services.context_limit {
        run = run.with_context_limit(limit);
    }
    let mut request = AdvanceRequest::new(
        run,
        services.configuration.sha256,
        services.steps_per_advance,
    )
    .map_err(WorkerError::Advance)?;
    if let Some(checkpoint) = &execution.job.checkpoint {
        request = request.resume(
            serde_json::from_value(checkpoint.reference.clone())
                .map_err(|_| WorkerError::Configuration)?,
        );
    }
    let (sender, mut receiver) = event_channel();
    let mut context = TurnContext::from_persistence(
        state,
        &services.providers,
        services.resolver.as_ref(),
        services.dispatcher.as_ref(),
        interrupt,
    )
    .with_principal_scope(execution.job.principal.clone())
    .with_budget_policy(services.budget);
    if let Some(refresher) = &services.dynamic_context_refresher {
        context = context.with_dynamic_context_refresher(refresher.as_ref());
    }
    let advance = services.driver.advance(request, context, sender);
    tokio::pin!(advance);
    loop {
        tokio::select! {
            biased;
            result = &mut advance => {
                // A profile may retain a sender beyond the advance. Its
                // lifecycle must not hold a committed scheduling boundary.
                for _ in 0..zuno_engine::r#loop::TURN_EVENT_CHANNEL_CAPACITY {
                    let Ok(event) = receiver.try_recv() else { break };
                    observer.event(&execution.job.id, event);
                }
                return result.map_err(WorkerError::Advance);
            }
            event = receiver.recv() => {
                match event {
                    Some(event) => observer.event(&execution.job.id, event),
                    None => return advance.await.map_err(WorkerError::Advance),
                }
            }
        }
    }
}
