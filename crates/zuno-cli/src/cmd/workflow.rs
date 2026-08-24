//! Durable execution of configuration-owned multi-agent workflow DAGs.
//!
//! The tool layer has already fixed the graph, resolved every node's model and
//! reasoning options, and checked permission/depth. This host owns only effects:
//! bounded overlap, cancellation propagation, durable parent jobs, stable result
//! ordering, and restart reconciliation without replay.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rusqlite::{OptionalExtension, params};

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::task::JoinSet;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use zuno_db::inbox::{InputDelivery, NewSessionInput};
use zuno_db::job::{
    AgentJob, AgentJobStore, JobSettlement, JobSubject, NewAgentJob,
    ReportDelivery as DbReportDelivery,
};
use zuno_db::pool::Pool;
use zuno_tools::task::{ChildTurn, ChildTurnRequest, ReportDelivery};
use zuno_tools::work_state::{
    WorkItem, WorkItemChange, WorkItemPriority, WorkItemStatus, WorkStateStore,
};
use zuno_tools::workflow::{WorkflowHost, WorkflowRequest, WorkflowTurn};

use super::child_turn::{BackgroundJobSupervisor, ChildSessionHost, ParentReportWake};

const CANCEL_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
type NodeJoin = (usize, String, String, i64, Result<ChildTurn, String>);

#[async_trait]
trait WorkflowNodeRunner: Send + Sync + 'static {
    async fn run(
        &self,
        request: ChildTurnRequest,
        cancellation: CancellationToken,
    ) -> Result<ChildTurn, String>;
}

#[async_trait]
impl WorkflowNodeRunner for ChildSessionHost {
    async fn run(
        &self,
        request: ChildTurnRequest,
        cancellation: CancellationToken,
    ) -> Result<ChildTurn, String> {
        self.dispatch_foreground(request, cancellation)
            .await
            .map_err(|error| error.to_string())
    }
}

#[derive(Debug, Clone)]
struct NodeResult {
    index: usize,
    id: String,
    agent: String,
    session_id: String,
    output: String,
}

#[derive(Debug, Clone)]
struct TrackedWorkItem {
    id: String,
    revision: i64,
    status: WorkItemStatus,
    started: Option<Instant>,
    tokens_used: Option<i64>,
}

#[derive(Debug)]
struct WorkflowItems {
    root: TrackedWorkItem,
    nodes: Vec<TrackedWorkItem>,
    started: Instant,
}

impl NodeResult {
    fn as_json(&self) -> Value {
        json!({
            "id":self.id,
            "agent":self.agent,
            "sessionID":self.session_id,
            "output":self.output,
        })
    }
}

enum WorkflowOutcome {
    Completed(Vec<NodeResult>),
    Failed {
        message: String,
        completed: Vec<NodeResult>,
    },
    Cancelled {
        message: String,
        completed: Vec<NodeResult>,
    },
    Uncertain {
        message: String,
        completed: Vec<NodeResult>,
    },
}

impl WorkflowOutcome {
    fn status(&self) -> &'static str {
        match self {
            Self::Completed(_) => "completed",
            Self::Failed { .. } => "failed",
            Self::Cancelled { .. } => "cancelled",
            Self::Uncertain { .. } => "uncertain",
        }
    }

    fn completed(&self) -> &[NodeResult] {
        match self {
            Self::Completed(nodes)
            | Self::Failed {
                completed: nodes, ..
            }
            | Self::Cancelled {
                completed: nodes, ..
            }
            | Self::Uncertain {
                completed: nodes, ..
            } => nodes,
        }
    }

    fn message(&self) -> Option<&str> {
        match self {
            Self::Completed(_) => None,
            Self::Failed { message, .. }
            | Self::Cancelled { message, .. }
            | Self::Uncertain { message, .. } => Some(message),
        }
    }

    fn summary(&self, workflow: &str) -> String {
        let mut lines = match self {
            Self::Completed(nodes) => vec![format!(
                "Workflow `{workflow}` completed {} node(s).",
                nodes.len()
            )],
            Self::Failed { message, completed } => vec![format!(
                "Workflow `{workflow}` failed after {} completed node(s): {message}",
                completed.len()
            )],
            Self::Cancelled { message, completed } => vec![format!(
                "Workflow `{workflow}` was cancelled after {} completed node(s): {message}",
                completed.len()
            )],
            Self::Uncertain { message, completed } => vec![format!(
                "Workflow `{workflow}` has an uncertain outcome after {} completed node(s): {message}",
                completed.len()
            )],
        };
        for node in self.completed() {
            lines.push(format!(
                "\n### {} ({})\n{}",
                node.id, node.agent, node.output
            ));
        }
        lines.join("\n")
    }

    fn result(&self, run_id: &str, workflow: &str) -> Option<Value> {
        matches!(self, Self::Completed(_)).then(|| {
            json!({
                "runID":run_id,
                "workflow":workflow,
                "nodes":self.completed().iter().map(NodeResult::as_json).collect::<Vec<_>>()
            })
        })
    }
}

/// Workflow scheduler and durable background effects for one turn host.
#[derive(Clone)]
pub(crate) struct NativeWorkflowHost {
    runner: Arc<dyn WorkflowNodeRunner>,
    database: Arc<Pool>,
    jobs: AgentJobStore,
    work: WorkStateStore,
    changes: super::child_turn::ChangeNotifier,
    wake: Arc<dyn ParentReportWake>,
    supervisor: BackgroundJobSupervisor,
}

impl NativeWorkflowHost {
    pub(crate) fn new(
        database: Arc<zuno_db::pool::Pool>,
        child: ChildSessionHost,
        wake: Arc<dyn ParentReportWake>,
        supervisor: BackgroundJobSupervisor,
    ) -> Self {
        let changes = supervisor.notifier();
        Self {
            runner: Arc::new(child),
            database: Arc::clone(&database),
            jobs: AgentJobStore::new(Arc::clone(&database)),
            work: WorkStateStore::new(database),
            changes,
            wake,
            supervisor,
        }
    }

    /// Mark process-owned workflow schedulers left running across restart uncertain.
    pub(crate) async fn recover_uncertain(&self, parent_session_id: &str) -> Result<usize, String> {
        let running = self
            .jobs
            .running_workflows_for(parent_session_id)
            .map_err(to_string)?;
        let mut recovered = 0_usize;
        for job in running {
            let JobSubject::Workflow { run_id, .. } = &job.subject else {
                continue;
            };
            let item_error = self
                .reconcile_uncertain_items(parent_session_id, run_id)
                .err();
            let mut message = format!(
                "Workflow job `{}` has an uncertain outcome because its Zuno scheduler was lost; completed node side effects are not replayed",
                job.id
            );
            if let Some(error) = item_error {
                message.push_str(&format!("; WorkItem reconciliation also failed: {error}"));
            }
            let completed = zuno_db::message::now_millis();
            let report = report_for_job(&job, "uncertain", &message, completed);
            let settled = self
                .jobs
                .settle(
                    &job.id,
                    JobSettlement::uncertain(message, completed, report),
                )
                .map_err(to_string)?;
            self.changes.changed();
            if let Some(report) = settled.report {
                self.wake.wake(report).await?;
            }
            recovered = recovered.saturating_add(1);
        }
        Ok(recovered)
    }

    fn active_goal_id(&self, session_id: &str) -> Result<Option<String>, String> {
        let connection = self.database.get().map_err(to_string)?;
        let exists = connection
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type=?1 AND name=?2",
                params!["table", "goal"],
                |_| Ok(()),
            )
            .optional()
            .map_err(to_string)?
            .is_some();
        if !exists {
            return Ok(None);
        }
        connection
            .query_row(
                "SELECT goal_id FROM goal WHERE session_id=?1 AND status != ?2 AND status != ?3",
                params![session_id, "complete", "cancelled"],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(to_string)
    }

    fn admit_work_items(
        &self,
        request: &WorkflowRequest,
        run_id: &str,
    ) -> Result<WorkflowItems, String> {
        let plan = self
            .work
            .plan(&request.parent_session_id)
            .map_err(to_string)?;
        let goal_id = match plan.as_ref().and_then(|plan| plan.goal_id.clone()) {
            Some(goal_id) => Some(goal_id),
            None => self.active_goal_id(&request.parent_session_id)?,
        };
        let plan_steps = plan
            .as_ref()
            .map(|plan| {
                plan.steps
                    .iter()
                    .map(|step| step.id.as_str())
                    .collect::<BTreeSet<_>>()
            })
            .unwrap_or_default();
        let root_id = workflow_root_item_id(run_id);
        let node_ids = request
            .nodes
            .iter()
            .enumerate()
            .map(|(index, node)| (node.id.as_str(), workflow_node_item_id(run_id, index)))
            .collect::<BTreeMap<_, _>>();
        let mut changes = vec![WorkItemChange::Add {
            id: Some(root_id.clone()),
            goal_id: goal_id.clone(),
            plan_step_id: None,
            parent_id: None,
            subject: format!("workflow {}", request.workflow),
            description: request
                .description
                .clone()
                .unwrap_or_else(|| format!("Configured multi-agent workflow {}", request.workflow)),
            active_form: Some(format!("Running {} workflow", request.workflow)),
            status: WorkItemStatus::InProgress,
            priority: WorkItemPriority::High,
            dependencies: Vec::new(),
            owner: Some("workflow".to_owned()),
        }];
        for (index, node) in request.nodes.iter().enumerate() {
            let dependencies = node
                .depends_on
                .iter()
                .map(|dependency| {
                    node_ids.get(dependency.as_str()).cloned().ok_or_else(|| {
                        format!(
                            "workflow node `{}` references unknown dependency `{dependency}`",
                            node.id
                        )
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            changes.push(WorkItemChange::Add {
                id: Some(workflow_node_item_id(run_id, index)),
                goal_id: goal_id.clone(),
                plan_step_id: plan_steps
                    .contains(node.id.as_str())
                    .then(|| node.id.clone()),
                parent_id: Some(root_id.clone()),
                subject: node.id.clone(),
                description: node
                    .turn
                    .description
                    .clone()
                    .unwrap_or_else(|| format!("{} workflow node {}", request.workflow, node.id)),
                active_form: Some(format!("Running {}", node.id)),
                status: WorkItemStatus::Pending,
                priority: WorkItemPriority::Medium,
                dependencies,
                owner: Some(node.turn.agent.clone()),
            });
        }
        let admitted = self
            .work
            .update_items(&request.parent_session_id, changes)
            .map_err(to_string)?;
        let mut by_id = admitted
            .into_iter()
            .map(|item| (item.id.clone(), item))
            .collect::<BTreeMap<_, _>>();
        let root = tracked_item(
            by_id
                .remove(&root_id)
                .ok_or_else(|| format!("workflow root WorkItem `{root_id}` was not persisted"))?,
            Some(Instant::now()),
        );
        let nodes = (0..request.nodes.len())
            .map(|index| {
                let id = workflow_node_item_id(run_id, index);
                by_id
                    .remove(&id)
                    .map(|item| tracked_item(item, None))
                    .ok_or_else(|| format!("workflow node WorkItem `{id}` was not persisted"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        self.changes.changed();
        Ok(WorkflowItems {
            root,
            nodes,
            started: Instant::now(),
        })
    }

    fn transition_item(
        &self,
        session_id: &str,
        item: &mut TrackedWorkItem,
        status: WorkItemStatus,
        elapsed_ms: Option<i64>,
        tokens_used: Option<i64>,
    ) -> Result<(), String> {
        let updated = self
            .work
            .transition_runtime_item(
                session_id,
                &item.id,
                item.revision,
                status,
                elapsed_ms,
                tokens_used,
            )
            .map_err(to_string)?;
        item.revision = updated.revision;
        item.status = updated.status;
        item.tokens_used = updated.usage_known.then_some(updated.tokens_used);
        self.changes.changed();
        Ok(())
    }

    fn child_tokens(&self, session_id: &str) -> Result<Option<i64>, String> {
        let connection = self.database.get().map_err(to_string)?;
        let session = match zuno_db::session::get(&connection, session_id) {
            Ok(session) => session,
            Err(zuno_error::DbError::NotFound { .. }) => return Ok(None),
            Err(error) => return Err(error.to_string()),
        };
        if !session.usage.known {
            return Ok(None);
        }
        let tokens = session.usage.tokens;
        Ok(Some(
            tokens
                .input
                .saturating_add(tokens.output)
                .saturating_add(tokens.reasoning)
                .saturating_add(tokens.cache_read)
                .saturating_add(tokens.cache_write)
                .max(0),
        ))
    }

    async fn execute_managed(
        &self,
        request: &WorkflowRequest,
        cancellation: CancellationToken,
        items: &mut WorkflowItems,
    ) -> WorkflowOutcome {
        let outcome = self.execute(request, cancellation, items).await;
        if let Err(error) = self.finalize_work_items(request, items, &outcome) {
            return WorkflowOutcome::Uncertain {
                message: format!(
                    "{}; durable WorkItem settlement failed: {error}",
                    outcome
                        .message()
                        .unwrap_or("workflow execution completed but its work state did not")
                ),
                completed: outcome.completed().to_vec(),
            };
        }
        outcome
    }

    fn finalize_work_items(
        &self,
        request: &WorkflowRequest,
        items: &mut WorkflowItems,
        outcome: &WorkflowOutcome,
    ) -> Result<(), String> {
        let node_status = match outcome {
            WorkflowOutcome::Completed(_) => WorkItemStatus::Completed,
            WorkflowOutcome::Cancelled { .. } => WorkItemStatus::Cancelled,
            WorkflowOutcome::Failed { .. } | WorkflowOutcome::Uncertain { .. } => {
                WorkItemStatus::Blocked
            }
        };
        let mut errors = Vec::new();
        for item in &mut items.nodes {
            if work_item_terminal(item.status) {
                continue;
            }
            let elapsed = item.started.map(elapsed_millis).unwrap_or_default();
            if let Err(error) = self.transition_item(
                &request.parent_session_id,
                item,
                node_status,
                Some(elapsed),
                None,
            ) {
                errors.push(error);
            }
        }
        let root_status = match outcome {
            WorkflowOutcome::Completed(_) => WorkItemStatus::Completed,
            WorkflowOutcome::Cancelled { .. } => WorkItemStatus::Cancelled,
            WorkflowOutcome::Failed { .. } | WorkflowOutcome::Uncertain { .. } => {
                WorkItemStatus::Blocked
            }
        };
        let root_tokens = matches!(outcome, WorkflowOutcome::Completed(_))
            .then(|| {
                items
                    .nodes
                    .iter()
                    .map(|item| item.tokens_used)
                    .collect::<Option<Vec<_>>>()
                    .map(|tokens| tokens.into_iter().fold(0_i64, i64::saturating_add))
            })
            .flatten();
        if !work_item_terminal(items.root.status)
            && let Err(error) = self.transition_item(
                &request.parent_session_id,
                &mut items.root,
                root_status,
                Some(elapsed_millis(items.started)),
                root_tokens,
            )
        {
            errors.push(error);
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }

    fn reconcile_uncertain_items(&self, session_id: &str, run_id: &str) -> Result<(), String> {
        let root_id = workflow_root_item_id(run_id);
        let now = zuno_db::message::now_millis();
        let mut errors = Vec::new();
        let mut changed = false;
        for item in self.work.items(session_id).map_err(to_string)? {
            if item.id != root_id && item.parent_id.as_deref() != Some(root_id.as_str()) {
                continue;
            }
            if work_item_terminal(item.status) {
                continue;
            }
            let elapsed = now.saturating_sub(item.time_created).max(0);
            if let Err(error) = self.work.transition_runtime_item(
                session_id,
                &item.id,
                item.revision,
                WorkItemStatus::Blocked,
                Some(elapsed),
                None,
            ) {
                errors.push(error.to_string());
            } else {
                changed = true;
            }
        }
        if changed {
            self.changes.changed();
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }

    async fn execute(
        &self,
        request: &WorkflowRequest,
        cancellation: CancellationToken,
        items: &mut WorkflowItems,
    ) -> WorkflowOutcome {
        let mut pending = (0..request.nodes.len()).collect::<BTreeSet<_>>();
        let mut completed_ids = BTreeSet::new();
        let mut results = vec![None; request.nodes.len()];

        while !pending.is_empty() {
            if cancellation.is_cancelled() {
                return WorkflowOutcome::Cancelled {
                    message: "cancelled before the next workflow node was admitted".to_owned(),
                    completed: ordered_results(&results),
                };
            }
            let ready = pending
                .iter()
                .copied()
                .filter(|index| {
                    request.nodes[*index]
                        .depends_on
                        .iter()
                        .all(|dependency| completed_ids.contains(dependency))
                })
                .take(request.max_parallel.max(1))
                .collect::<Vec<_>>();
            if ready.is_empty() {
                return WorkflowOutcome::Failed {
                    message: "no runnable node remains; dependencies did not reach a successful terminal state"
                        .to_owned(),
                    completed: ordered_results(&results),
                };
            }
            for index in &ready {
                pending.remove(index);
                if let Err(error) = self.transition_item(
                    &request.parent_session_id,
                    &mut items.nodes[*index],
                    WorkItemStatus::InProgress,
                    None,
                    None,
                ) {
                    return WorkflowOutcome::Failed {
                        message: format!(
                            "workflow node `{}` could not enter running state: {error}",
                            request.nodes[*index].id
                        ),
                        completed: ordered_results(&results),
                    };
                }
                items.nodes[*index].started = Some(Instant::now());
            }

            let batch_cancellation = cancellation.child_token();
            let mut tasks = JoinSet::new();
            for index in ready {
                let node = request.nodes[index].clone();
                let runner = Arc::clone(&self.runner);
                let node_cancellation = batch_cancellation.child_token();
                tasks.spawn(async move {
                    let id = node.id;
                    let agent = node.turn.agent.clone();
                    let started = Instant::now();
                    let result = runner.run(node.turn, node_cancellation).await;
                    (index, id, agent, elapsed_millis(started), result)
                });
            }

            while !tasks.is_empty() {
                let joined = tokio::select! {
                    () = cancellation.cancelled() => {
                        batch_cancellation.cancel();
                        if drain_cancelled(&mut tasks).await {
                            return WorkflowOutcome::Cancelled {
                                message: "cancelled by the parent turn".to_owned(),
                                completed: ordered_results(&results),
                            };
                        }
                        return WorkflowOutcome::Uncertain {
                            message: "node tasks did not acknowledge cancellation before the safety timeout"
                                .to_owned(),
                            completed: ordered_results(&results),
                        };
                    }
                    joined = tasks.join_next() => joined,
                };
                let Some(joined) = joined else {
                    break;
                };
                match joined {
                    Ok((index, id, agent, elapsed_ms, Ok(turn))) => {
                        completed_ids.insert(id.clone());
                        let session_id = turn.session_id;
                        results[index] = Some(NodeResult {
                            index,
                            id,
                            agent,
                            session_id: session_id.clone(),
                            output: turn.output,
                        });
                        let tokens = match self.child_tokens(&session_id) {
                            Ok(tokens) => tokens,
                            Err(error) => {
                                return WorkflowOutcome::Uncertain {
                                    message: format!(
                                        "workflow node usage could not be reconciled after completion: {error}"
                                    ),
                                    completed: ordered_results(&results),
                                };
                            }
                        };
                        if let Err(error) = self.transition_item(
                            &request.parent_session_id,
                            &mut items.nodes[index],
                            WorkItemStatus::Completed,
                            Some(elapsed_ms),
                            tokens,
                        ) {
                            return WorkflowOutcome::Uncertain {
                                message: format!(
                                    "workflow node `{}` completed but its WorkItem could not be settled: {error}",
                                    request.nodes[index].id
                                ),
                                completed: ordered_results(&results),
                            };
                        }
                    }
                    Ok((_index, id, _agent, _elapsed_ms, Err(error))) => {
                        batch_cancellation.cancel();
                        if !drain_cancelled(&mut tasks).await {
                            return WorkflowOutcome::Uncertain {
                                message: format!(
                                    "node `{id}` failed ({error}) and sibling cancellation could not be confirmed"
                                ),
                                completed: ordered_results(&results),
                            };
                        }
                        if cancellation.is_cancelled() {
                            return WorkflowOutcome::Cancelled {
                                message: "cancelled by the parent turn".to_owned(),
                                completed: ordered_results(&results),
                            };
                        }
                        return WorkflowOutcome::Failed {
                            message: format!("node `{id}` failed: {error}"),
                            completed: ordered_results(&results),
                        };
                    }
                    Err(error) => {
                        batch_cancellation.cancel();
                        let _drained = drain_cancelled(&mut tasks).await;
                        return WorkflowOutcome::Uncertain {
                            message: format!("a workflow node task was lost: {error}"),
                            completed: ordered_results(&results),
                        };
                    }
                }
            }
        }

        WorkflowOutcome::Completed(ordered_results(&results))
    }

    async fn settle(
        &self,
        request: &WorkflowRequest,
        run_id: &str,
        job_id: &str,
        outcome: &WorkflowOutcome,
    ) -> Result<String, String> {
        let completed = zuno_db::message::now_millis();
        let summary = outcome.summary(&request.workflow);
        let report = report_for_request(
            request,
            job_id,
            run_id,
            outcome.status(),
            &summary,
            completed,
        );
        let settlement = match outcome {
            WorkflowOutcome::Completed(_) => JobSettlement::completed(
                outcome
                    .result(run_id, &request.workflow)
                    .expect("completed workflows have a result"),
                completed,
                report,
            ),
            WorkflowOutcome::Failed { .. } => JobSettlement::failed(
                outcome.message().unwrap_or("workflow failed"),
                completed,
                report,
            ),
            WorkflowOutcome::Cancelled { .. } => JobSettlement::cancelled(
                outcome.message().unwrap_or("workflow cancelled"),
                completed,
                report,
            ),
            WorkflowOutcome::Uncertain { .. } => JobSettlement::uncertain(
                outcome.message().unwrap_or("workflow outcome uncertain"),
                completed,
                report,
            ),
        };
        let settled = self.jobs.settle(job_id, settlement).map_err(to_string)?;
        self.changes.changed();
        if let Some(report) = settled.report {
            self.wake.wake(report).await?;
        }
        Ok(summary)
    }
}

#[async_trait]
impl WorkflowHost for NativeWorkflowHost {
    async fn dispatch(
        &self,
        request: WorkflowRequest,
        cancellation: CancellationToken,
    ) -> Result<WorkflowTurn, String> {
        let run_id = super::turn::prefixed_id("run");
        let job_id = super::turn::prefixed_id("job");
        let delivery = if request.background {
            db_delivery(request.report_delivery)
        } else {
            DbReportDelivery::Quiet
        };
        self.jobs
            .create(
                NewAgentJob::new(
                    job_id.clone(),
                    request.parent_session_id.clone(),
                    JobSubject::workflow(run_id.clone(), request.workflow.clone()),
                    delivery,
                    zuno_db::message::now_millis(),
                )
                .with_orchestration_snapshot(request.parent_attempt.as_deref().cloned()),
            )
            .map_err(to_string)?;
        let mut work_items = match self.admit_work_items(&request, &run_id) {
            Ok(items) => items,
            Err(error) => {
                let message = format!(
                    "workflow `{}` was not admitted because its durable WorkItems could not be created: {error}",
                    request.workflow
                );
                let _ = self.jobs.settle(
                    &job_id,
                    JobSettlement::failed(message.clone(), zuno_db::message::now_millis(), None),
                );
                self.changes.changed();
                return Err(message);
            }
        };

        if request.background {
            let host = self.clone();
            let background_job_id = job_id.clone();
            let background_run_id = run_id.clone();
            let parent_session_id = request.parent_session_id.clone();
            let task_cancellation = cancellation.clone();
            self.supervisor.spawn(
                job_id.clone(),
                parent_session_id,
                cancellation,
                async move {
                    let outcome = host
                        .execute_managed(&request, task_cancellation, &mut work_items)
                        .await;
                    if let Err(error) = host
                        .settle(&request, &background_run_id, &background_job_id, &outcome)
                        .await
                    {
                        tracing::error!(
                            job_id = %background_job_id,
                            %error,
                            "workflow job settlement failed"
                        );
                    }
                },
            );
            return Ok(WorkflowTurn {
                run_id,
                job_id: Some(job_id),
                output: "Workflow started. Its immutable DAG is running under the configured concurrency bound."
                    .to_owned(),
            });
        }

        let outcome = self
            .execute_managed(&request, cancellation, &mut work_items)
            .await;
        let summary = self.settle(&request, &run_id, &job_id, &outcome).await?;
        match outcome {
            WorkflowOutcome::Completed(_) => Ok(WorkflowTurn {
                run_id,
                job_id: None,
                output: summary,
            }),
            WorkflowOutcome::Failed { .. }
            | WorkflowOutcome::Cancelled { .. }
            | WorkflowOutcome::Uncertain { .. } => Err(summary),
        }
    }
}

fn workflow_root_item_id(run_id: &str) -> String {
    format!("work_{run_id}")
}

fn workflow_node_item_id(run_id: &str, index: usize) -> String {
    format!("{}:node:{index:04}", workflow_root_item_id(run_id))
}

fn tracked_item(item: WorkItem, started: Option<Instant>) -> TrackedWorkItem {
    TrackedWorkItem {
        id: item.id,
        revision: item.revision,
        status: item.status,
        started,
        tokens_used: item.usage_known.then_some(item.tokens_used),
    }
}

fn work_item_terminal(status: WorkItemStatus) -> bool {
    matches!(
        status,
        WorkItemStatus::Completed | WorkItemStatus::Cancelled | WorkItemStatus::Blocked
    )
}

fn elapsed_millis(started: Instant) -> i64 {
    i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX)
}

fn ordered_results(results: &[Option<NodeResult>]) -> Vec<NodeResult> {
    let mut nodes = results.iter().flatten().cloned().collect::<Vec<_>>();
    nodes.sort_by_key(|node| node.index);
    nodes
}

async fn drain_cancelled(tasks: &mut JoinSet<NodeJoin>) -> bool {
    if timeout(CANCEL_DRAIN_TIMEOUT, async {
        while tasks.join_next().await.is_some() {}
    })
    .await
    .is_ok()
    {
        return true;
    }
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    false
}

fn report_for_request(
    request: &WorkflowRequest,
    job_id: &str,
    run_id: &str,
    status: &str,
    text: &str,
    created: i64,
) -> Option<NewSessionInput> {
    (request.background && request.report_delivery == ReportDelivery::NextStep).then(|| {
        NewSessionInput::new(
            super::turn::prefixed_id("input"),
            request.parent_session_id.clone(),
            json!({
                "kind":"workflowReport",
                "jobID":job_id,
                "runID":run_id,
                "workflow":request.workflow,
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
    let JobSubject::Workflow { run_id, workflow } = &job.subject else {
        return None;
    };
    Some(NewSessionInput::new(
        super::turn::prefixed_id("input"),
        job.parent_session_id.clone(),
        json!({
            "kind":"workflowReport",
            "jobID":job.id,
            "runID":run_id,
            "workflow":workflow,
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
#[path = "workflow_tests.rs"]
mod tests;
