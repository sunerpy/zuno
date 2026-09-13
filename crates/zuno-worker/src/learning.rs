//! Isolated learning work shares Worker capacity and uses scoped state grants.
use crate::runtime::WorkerError;
use crate::*;
use async_trait::async_trait;
use std::{
    sync::{Arc, RwLock},
    time::Duration,
};
use zuno_application::{learning::LearningExecutionLease, runtime::ConfigurationRef};
use zuno_identity::worker::LearningGrantToken;
use zuno_learning::{
    LearningModelClient, LearningModelJournal, LearningModelRecord, distributed::*,
};

pub const LEARNING_CLAIM_PATH: &str = "internal/worker/v1/learning/claim";
pub const LEARNING_RENEW_PATH: &str = "internal/worker/v1/learning/renew";
pub const LEARNING_JOURNAL_PATH: &str = "internal/worker/v1/learning/journal";
pub const LEARNING_COMPLETE_PATH: &str = "internal/worker/v1/learning/complete";
pub const LEARNING_STOP_PATH: &str = "internal/worker/v1/learning/stop";
pub const LEARNING_GRANT_HEADER: &str = "x-zuno-learning-grant";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LearningClaimRequest {
    pub version: u32,
    pub worker: WorkerInstanceId,
    pub configurations: Vec<ConfigurationRef>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct IssuedLearning {
    pub claimed: ClaimedLearning,
    pub grant: LearningGrantToken,
    pub valid_for_ms: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RenewedLearning {
    pub lease: LearningExecutionLease,
    pub grant: LearningGrantToken,
    pub valid_for_ms: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LearningStopRequest {
    pub lease: LearningExecutionLease,
    pub stop: LearningStop,
}

struct Credential {
    lease: LearningExecutionLease,
    grant: LearningGrantToken,
    deadline: tokio::time::Instant,
}
struct Execution {
    job: LearningExecution,
    credential: RwLock<Credential>,
}
impl Execution {
    fn current(&self) -> Result<(LearningExecutionLease, LearningGrantToken), WorkerError> {
        let state = self.credential.read().map_err(|_| WorkerError::Task)?;
        if tokio::time::Instant::now() >= state.deadline {
            return Err(WorkerError::LeaseExpired);
        }
        Ok((state.lease.clone(), state.grant.clone()))
    }
}

#[async_trait]
pub trait LearningModelFactory: Send + Sync {
    fn learning_configurations(&self) -> Vec<ConfigurationRef>;
    async fn learning_model(
        &self,
        execution: &LearningExecution,
        journal: Arc<dyn LearningModelJournal>,
    ) -> Result<LearningModelClient, WorkerError>;
}

pub struct LearningWorker {
    client: WorkerClient,
    factory: Arc<dyn LearningModelFactory>,
    configurations: Vec<ConfigurationRef>,
}
impl LearningWorker {
    pub fn new(
        client: WorkerClient,
        factory: Arc<dyn LearningModelFactory>,
    ) -> Result<Self, WorkerError> {
        let configurations = factory.learning_configurations();
        crate::validate_configurations(&configurations).map_err(|_| WorkerError::Configuration)?;
        Ok(Self {
            client,
            factory,
            configurations,
        })
    }
}

async fn post<T: Serialize>(
    client: &WorkerClient,
    path: &str,
    grant: Option<&LearningGrantToken>,
    body: &T,
) -> Result<Vec<u8>, WorkerError> {
    client
        .post_header(
            path,
            grant.map(|g| (LEARNING_GRANT_HEADER, g.expose())),
            serde_json::to_vec(body).map_err(|_| WorkerError::Configuration)?,
        )
        .await
        .map_err(|error| match error {
            TurnStateError::Forbidden | TurnStateError::LeaseLost => WorkerError::LeaseLost,
            TurnStateError::Conflict | TurnStateError::InvalidData => WorkerError::Configuration,
            _ => WorkerError::Unavailable,
        })
}

struct RemoteJournal {
    client: WorkerClient,
    execution: Arc<Execution>,
}
#[async_trait]
impl LearningModelJournal for RemoteJournal {
    async fn record(&self, record: LearningModelRecord) -> zuno_learning::Result<()> {
        let starts_request = matches!(
            record.event,
            zuno_learning::LearningModelEvent::Request { .. }
        );
        let (lease, grant) = self
            .execution
            .current()
            .map_err(|_| zuno_memory::MemoryServiceError::Denied)?;
        post(
            &self.client,
            LEARNING_JOURNAL_PATH,
            Some(&grant),
            &LearningJournalRequest { lease, record },
        )
        .await
        .map_err(|error| match error {
            WorkerError::LeaseLost | WorkerError::LeaseExpired => {
                zuno_memory::MemoryServiceError::Denied
            }
            WorkerError::Configuration => zuno_memory::MemoryServiceError::Conflict,
            _ => zuno_memory::MemoryServiceError::Unavailable,
        })?;
        if starts_request {
            self.execution
                .current()
                .map_err(|_| zuno_memory::MemoryServiceError::Denied)?;
        }
        Ok(())
    }
}

#[async_trait]
impl crate::runtime::WorkerAuxiliary for LearningWorker {
    async fn claim(
        &self,
        worker: WorkerInstanceId,
        renew: Duration,
    ) -> Result<Option<crate::runtime::AuxiliaryTask>, WorkerError> {
        let started = tokio::time::Instant::now();
        let bytes = post(
            &self.client,
            LEARNING_CLAIM_PATH,
            None,
            &LearningClaimRequest {
                version: 1,
                worker: worker.clone(),
                configurations: self.configurations.clone(),
            },
        )
        .await?;
        let issued: Option<IssuedLearning> =
            serde_json::from_slice(&bytes).map_err(|_| WorkerError::Configuration)?;
        let Some(issued) = issued else {
            return Ok(None);
        };
        issued
            .claimed
            .lease
            .validate()
            .map_err(|_| WorkerError::Configuration)?;
        if issued.valid_for_ms == 0
            || issued.valid_for_ms > 300000
            || issued.claimed.lease.worker != worker
            || issued.claimed.lease.job_id != issued.claimed.execution.id
            || issued.claimed.lease.owner != issued.claimed.execution.principal.owner()
            || !self
                .configurations
                .contains(&issued.claimed.execution.configuration)
        {
            return Err(WorkerError::Configuration);
        }
        let execution = Arc::new(Execution {
            job: issued.claimed.execution,
            credential: RwLock::new(Credential {
                lease: issued.claimed.lease,
                grant: issued.grant,
                deadline: started + Duration::from_millis(issued.valid_for_ms),
            }),
        });
        let client = self.client.clone();
        let factory = self.factory.clone();
        Ok(Some(Box::pin(async move {
            let work = run_learning(&client, execution.clone(), factory.as_ref());
            tokio::pin!(work);
            let heartbeat = async {
                let mut ticks = tokio::time::interval(renew);
                ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                ticks.tick().await;
                loop {
                    let deadline = execution
                        .credential
                        .read()
                        .map_err(|_| WorkerError::Task)?
                        .deadline;
                    tokio::select! {
                        biased;
                        _=tokio::time::sleep_until(deadline)=>return Err(WorkerError::LeaseExpired),
                        _=ticks.tick()=>{}
                    }
                    let (lease, grant) = execution.current()?;
                    let started = tokio::time::Instant::now();
                    match tokio::time::timeout_at(
                        deadline,
                        post(&client, LEARNING_RENEW_PATH, Some(&grant), &lease),
                    )
                    .await
                    .map_err(|_| WorkerError::LeaseExpired)?
                    {
                        Ok(bytes) => {
                            let issued: RenewedLearning = serde_json::from_slice(&bytes)
                                .map_err(|_| WorkerError::Configuration)?;
                            if issued.lease.owner != lease.owner
                                || issued.lease.job_id != lease.job_id
                                || issued.lease.worker != lease.worker
                                || issued.lease.token != lease.token
                                || issued.lease.epoch != lease.epoch
                                || issued.valid_for_ms == 0
                                || issued.valid_for_ms > 300000
                            {
                                return Err(WorkerError::Configuration);
                            }
                            let mut state = execution
                                .credential
                                .write()
                                .map_err(|_| WorkerError::Task)?;
                            state.lease = issued.lease;
                            state.grant = issued.grant;
                            state.deadline = started + Duration::from_millis(issued.valid_for_ms);
                        }
                        Err(WorkerError::Unavailable) => {}
                        Err(error) => return Err(error),
                    }
                }
            };
            tokio::pin!(heartbeat);
            tokio::select! {
                biased;
                result=&mut work=>result,
                result=&mut heartbeat=>result,
            }
        })))
    }
}

async fn run_learning(
    client: &WorkerClient,
    execution: Arc<Execution>,
    factory: &dyn LearningModelFactory,
) -> Result<(), WorkerError> {
    use zuno_learning::{LearningExtractor, MemoryConsolidator};
    let journal = Arc::new(RemoteJournal {
        client: client.clone(),
        execution: execution.clone(),
    });
    let model = factory.learning_model(&execution.job, journal).await?;
    let result = if let Some(result) = &execution.job.cached_output {
        Ok(result.clone())
    } else {
        match &execution.job.input {
            LearningInput::Extraction(input) => model
                .extract(input.clone())
                .await
                .map(LearningOutput::Extraction),
            LearningInput::Maintenance(input) => model
                .consolidate_memory(input.clone())
                .await
                .map(LearningOutput::Maintenance),
        }
    };
    let (lease, grant) = execution.current()?;
    match result {
        Ok(result) => {
            match post(
                client,
                LEARNING_COMPLETE_PATH,
                Some(&grant),
                &LearningCompletion {
                    lease: lease.clone(),
                    result,
                },
            )
            .await
            {
                Ok(_) => Ok(()),
                Err(error @ WorkerError::Configuration) | Err(error @ WorkerError::Unavailable) => {
                    let stop = if matches!(error, WorkerError::Configuration) {
                        LearningStop::Failed {
                            code: "learning_settlement".to_owned(),
                            detail: "Learning output or frozen inputs no longer validate."
                                .to_owned(),
                        }
                    } else {
                        LearningStop::Retry {
                            after_ms: None,
                            detail: "Learning settlement acknowledgement is unavailable."
                                .to_owned(),
                        }
                    };
                    let _ = post(
                        client,
                        LEARNING_STOP_PATH,
                        Some(&grant),
                        &LearningStopRequest { lease, stop },
                    )
                    .await;
                    Err(error)
                }
                Err(error) => Err(error),
            }
        }
        Err(error) => {
            let stop = match error.recovery() {
                zuno_error::Recovery::Retry { after } => LearningStop::Retry {
                    after_ms: after
                        .map(|value| u64::try_from(value.as_millis()).unwrap_or(u64::MAX)),
                    detail: error.diagnostic(),
                },
                _ => LearningStop::Failed {
                    code: "learning_model".to_owned(),
                    detail: error.diagnostic(),
                },
            };
            post(
                client,
                LEARNING_STOP_PATH,
                Some(&grant),
                &LearningStopRequest { lease, stop },
            )
            .await?;
            Ok(())
        }
    }
}
