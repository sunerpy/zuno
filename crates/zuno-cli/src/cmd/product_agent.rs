//! Composition-root host for configured Codex and Claude Code product agents.
//!
//! Native product protocols live in `zuno-product-agent`; durable ownership,
//! background reporting, restart reconciliation, and cancellation live here beside
//! the child-session host that already owns those session effects.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;
use tokio_util::sync::CancellationToken;
use zuno_config::schema::Config;
use zuno_db::inbox::{InputDelivery, NewSessionInput};
use zuno_db::job::{
    AgentJob, AgentJobStore, JobSettlement, JobSubject, NewAgentJob,
    ReportDelivery as DbReportDelivery,
};
use zuno_product_agent::{ProductAgent, ProductAgentError, ProductAgentRequest as NativeRequest};
use zuno_tools::product_agent::{
    ProductAgentHost, ProductAgentRequest, ProductAgentTurn, product_id,
};
use zuno_tools::task::ReportDelivery;

use super::child_turn::{BackgroundJobSupervisor, ParentReportWake};

struct ConfiguredProduct {
    product: String,
    tool: String,
    runner: Arc<dyn ProductAgent>,
}

/// Product-agent execution and durable background effects for one turn host.
#[derive(Clone)]
pub(crate) struct NativeProductAgentHost {
    agents: Arc<BTreeMap<String, ConfiguredProduct>>,
    directory: PathBuf,
    jobs: AgentJobStore,
    wake: Arc<dyn ParentReportWake>,
    supervisor: BackgroundJobSupervisor,
}

impl NativeProductAgentHost {
    /// Resolve every enabled instance without touching native credentials.
    pub(crate) fn new(
        config: &Config,
        environment: &zuno_paths::Env,
        directory: PathBuf,
        database: Arc<zuno_db::pool::Pool>,
        wake: Arc<dyn ParentReportWake>,
        supervisor: BackgroundJobSupervisor,
    ) -> Result<Self, String> {
        let mut agents = BTreeMap::new();
        for (instance, configured) in config.product_agent.iter().flatten() {
            if !configured.is_enabled() {
                continue;
            }
            configured.validate(instance)?;
            let runner = zuno_product_agent::configured(instance, configured, environment)?;
            agents.insert(
                instance.to_owned(),
                ConfiguredProduct {
                    product: product_id(configured.kind).to_owned(),
                    tool: configured.resolved_tool_name().to_owned(),
                    runner,
                },
            );
        }
        Ok(Self {
            agents: Arc::new(agents),
            directory,
            jobs: AgentJobStore::new(database),
            wake,
            supervisor,
        })
    }

    /// Mark process-owned invocations left running across a restart as uncertain.
    ///
    /// They are never replayed: the native product may have performed side effects
    /// before its stdio stream disappeared.
    pub(crate) async fn recover_uncertain(&self, parent_session_id: &str) -> Result<usize, String> {
        let running = self
            .jobs
            .running_product_agents_for(parent_session_id)
            .map_err(to_string)?;
        let mut recovered = 0_usize;
        for job in running {
            let message = format!(
                "Background product agent has an uncertain outcome for job `{}`: the Zuno process \
                 restarted or lost the native product executor; the external outcome is unknown \
                 and this invocation will not be replayed",
                job.id
            );
            let completed = zuno_db::message::now_millis();
            let report = report_for_job(&job, "uncertain", &message, completed);
            let settled = self
                .jobs
                .settle(
                    &job.id,
                    JobSettlement::uncertain(message, completed, report),
                )
                .map_err(to_string)?;
            if let Some(report) = settled.report {
                self.wake.wake(report).await?;
            }
            recovered = recovered.saturating_add(1);
        }
        Ok(recovered)
    }

    fn configured(&self, request: &ProductAgentRequest) -> Result<&ConfiguredProduct, String> {
        let configured = self.agents.get(&request.instance).ok_or_else(|| {
            format!(
                "product-agent instance `{}` is not enabled in this host",
                request.instance
            )
        })?;
        if configured.product != request.product || configured.tool != request.tool {
            return Err(format!(
                "product-agent instance `{}` is registered as {}/{} but the tool requested {}/{}",
                request.instance,
                configured.product,
                configured.tool,
                request.product,
                request.tool
            ));
        }
        Ok(configured)
    }
}

#[async_trait]
impl ProductAgentHost for NativeProductAgentHost {
    async fn dispatch(
        &self,
        request: ProductAgentRequest,
        cancellation: CancellationToken,
    ) -> Result<ProductAgentTurn, String> {
        let configured = self.configured(&request)?;
        let runner = Arc::clone(&configured.runner);
        let run_id = super::turn::prefixed_id("run");
        let native = NativeRequest {
            prompt: request.prompt.clone(),
            description: request.description.clone(),
            directory: self.directory.clone(),
        };
        if !request.background {
            let result = runner.run(native, cancellation).await.map_err(to_string)?;
            return Ok(ProductAgentTurn {
                run_id,
                job_id: None,
                output: result.text,
            });
        }

        let job_id = super::turn::prefixed_id("job");
        let delivery = db_delivery(request.report_delivery);
        self.jobs
            .create(NewAgentJob::new(
                job_id.clone(),
                request.parent_session_id.clone(),
                JobSubject::product_agent(
                    run_id.clone(),
                    request.product.clone(),
                    request.instance.clone(),
                    request.tool.clone(),
                ),
                delivery,
                zuno_db::message::now_millis(),
            ))
            .map_err(to_string)?;

        let jobs = self.jobs.clone();
        let wake = Arc::clone(&self.wake);
        let background_job_id = job_id.clone();
        let background_run_id = run_id.clone();
        let parent_session_id = request.parent_session_id.clone();
        let task_cancellation = CancellationToken::new();
        let runner_cancellation = task_cancellation.clone();
        self.supervisor.spawn(
            job_id.clone(),
            parent_session_id,
            task_cancellation,
            async move {
                let outcome = runner.run(native, runner_cancellation).await;
                settle_background_product(
                    jobs,
                    wake,
                    request,
                    background_job_id,
                    background_run_id,
                    outcome,
                )
                .await;
            },
        );
        Ok(ProductAgentTurn {
            run_id,
            job_id: Some(job_id),
            output: "Product subagent started. Its terminal state will be delivered according to \
                     `reportDelivery`."
                .to_owned(),
        })
    }
}

async fn settle_background_product(
    jobs: AgentJobStore,
    wake: Arc<dyn ParentReportWake>,
    request: ProductAgentRequest,
    job_id: String,
    run_id: String,
    outcome: Result<zuno_product_agent::ProductAgentResult, ProductAgentError>,
) {
    let completed = zuno_db::message::now_millis();
    let (status, summary, settlement) = match outcome {
        Ok(result) => {
            let summary = format!(
                "Background {} product agent `{}` completed job `{job_id}`.\n\n{}",
                request.product, request.instance, result.text
            );
            let report =
                report_for_request(&request, &job_id, &run_id, "completed", &summary, completed);
            (
                "completed",
                summary,
                JobSettlement::completed(
                    json!({"text":result.text,"runID":run_id}),
                    completed,
                    report,
                ),
            )
        }
        Err(ProductAgentError::Cancelled { .. }) => {
            let summary = format!(
                "Background {} product agent `{}` cancelled job `{job_id}`.",
                request.product, request.instance
            );
            let report =
                report_for_request(&request, &job_id, &run_id, "cancelled", &summary, completed);
            (
                "cancelled",
                summary,
                JobSettlement::cancelled("cancelled by user", completed, report),
            )
        }
        Err(error) if error.is_uncertain() => {
            let error = error.to_string();
            let summary = format!(
                "Background {} product agent `{}` has an uncertain outcome for job `{job_id}`: \
                 {error}",
                request.product, request.instance
            );
            let report =
                report_for_request(&request, &job_id, &run_id, "uncertain", &summary, completed);
            (
                "uncertain",
                summary,
                JobSettlement::uncertain(error, completed, report),
            )
        }
        Err(error) => {
            let error = error.to_string();
            let summary = format!(
                "Background {} product agent `{}` failed job `{job_id}`: {error}",
                request.product, request.instance
            );
            let report =
                report_for_request(&request, &job_id, &run_id, "failed", &summary, completed);
            (
                "failed",
                summary,
                JobSettlement::failed(error, completed, report),
            )
        }
    };
    match jobs.settle(&job_id, settlement) {
        Ok(settled) => {
            if let Some(report) = settled.report
                && let Err(error) = wake.wake(report).await
            {
                tracing::error!(
                    job_id = %job_id,
                    %error,
                    "product-agent report remains pending after wake failure"
                );
            }
        }
        Err(error) => tracing::error!(
            job_id = %job_id,
            status,
            %error,
            report = %summary,
            "product-agent job settlement failed"
        ),
    }
}

fn report_for_request(
    request: &ProductAgentRequest,
    job_id: &str,
    run_id: &str,
    status: &str,
    text: &str,
    created: i64,
) -> Option<NewSessionInput> {
    (request.report_delivery == ReportDelivery::NextStep).then(|| {
        NewSessionInput::new(
            super::turn::prefixed_id("input"),
            request.parent_session_id.clone(),
            json!({
                "kind":"productAgentReport",
                "jobID":job_id,
                "runID":run_id,
                "product":request.product,
                "instance":request.instance,
                "tool":request.tool,
                "status":status,
                "text":text,
            }),
            InputDelivery::Queue,
            created,
        )
    })
}

fn report_for_job(
    job: &AgentJob,
    status: &str,
    text: &str,
    created: i64,
) -> Option<NewSessionInput> {
    if job.report_delivery != DbReportDelivery::NextStep {
        return None;
    }
    let JobSubject::ProductAgent {
        run_id,
        product,
        instance,
        tool,
    } = &job.subject
    else {
        return None;
    };
    Some(NewSessionInput::new(
        super::turn::prefixed_id("input"),
        job.parent_session_id.clone(),
        json!({
            "kind":"productAgentReport",
            "jobID":job.id,
            "runID":run_id,
            "product":product,
            "instance":instance,
            "tool":tool,
            "status":status,
            "text":text,
        }),
        InputDelivery::Queue,
        created,
    ))
}

fn db_delivery(delivery: ReportDelivery) -> DbReportDelivery {
    match delivery {
        ReportDelivery::NextStep => DbReportDelivery::NextStep,
        ReportDelivery::Quiet => DbReportDelivery::Quiet,
    }
}

fn to_string(error: impl std::fmt::Display) -> String {
    error.to_string()
}

#[cfg(test)]
#[path = "product_agent_tests.rs"]
mod tests;
