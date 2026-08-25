//! Durable execution of configuration-owned multi-agent workflow DAGs.
//!
//! The tool layer has already fixed the graph, resolved every node's model and
//! reasoning options, and checked permission/depth. This host owns only effects:
//! bounded work-conserving overlap, cancellation propagation, durable parent
//! jobs, stable result ordering, and restart reconciliation without replay.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rusqlite::{OptionalExtension, params};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
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
use zuno_engine::prelude::{InternalAgent, complete_internal_text};
use zuno_llm::event::{Message, Role};
use zuno_llm::registry::Provider;
use zuno_tools::council::{CouncilHost, CouncilRequest, CouncilSeatRequest, CouncilTurn};
use zuno_tools::task::{ChildTurn, ChildTurnRequest, ReportDelivery};
use zuno_tools::work_state::{
    WorkItem, WorkItemChange, WorkItemPriority, WorkItemStatus, WorkStateStore,
};
use zuno_tools::workflow::{WorkflowHost, WorkflowRequest, WorkflowTurn};

use super::child_turn::{BackgroundJobSupervisor, ChildSessionHost, ParentReportWake};

const CANCEL_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_COUNCIL_LIST_ITEMS: usize = 32;
const MAX_COUNCIL_FIELD_BYTES: usize = 4_096;
type NodeJoin = (usize, String, String, i64, Result<ChildTurn, String>);
type CouncilJoin = (usize, i64, CouncilSeatResult);

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum CouncilSeatStatus {
    Completed,
    Failed,
    Invalid,
    TimedOut,
    Cancelled,
    Uncertain,
}

impl CouncilSeatStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Invalid => "invalid",
            Self::TimedOut => "timed_out",
            Self::Cancelled => "cancelled",
            Self::Uncertain => "uncertain",
        }
    }

    fn work_item_status(self) -> WorkItemStatus {
        match self {
            Self::Completed => WorkItemStatus::Completed,
            Self::Cancelled => WorkItemStatus::Cancelled,
            Self::Failed | Self::Invalid | Self::TimedOut | Self::Uncertain => {
                WorkItemStatus::Blocked
            }
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CouncilSeatAnswer {
    verdict: String,
    confidence: f64,
    evidence: Vec<String>,
    risks: Vec<String>,
    recommendation: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct CouncilSeatResult {
    id: String,
    agent: String,
    status: CouncilSeatStatus,
    attempts: usize,
    session_id: Option<String>,
    verdict: Option<String>,
    confidence: Option<f64>,
    evidence: Vec<String>,
    risks: Vec<String>,
    recommendation: Option<String>,
    error: Option<String>,
}

impl CouncilSeatResult {
    fn completed(
        seat: &CouncilSeatRequest,
        attempts: usize,
        session_id: String,
        answer: CouncilSeatAnswer,
    ) -> Self {
        Self {
            id: seat.id.clone(),
            agent: seat.turn.agent.clone(),
            status: CouncilSeatStatus::Completed,
            attempts,
            session_id: Some(session_id),
            verdict: Some(answer.verdict),
            confidence: Some(answer.confidence),
            evidence: answer.evidence,
            risks: answer.risks,
            recommendation: Some(answer.recommendation),
            error: None,
        }
    }

    fn terminal(
        seat: &CouncilSeatRequest,
        status: CouncilSeatStatus,
        attempts: usize,
        session_id: Option<String>,
        error: impl Into<String>,
    ) -> Self {
        Self {
            id: seat.id.clone(),
            agent: seat.turn.agent.clone(),
            status,
            attempts,
            session_id,
            verdict: None,
            confidence: None,
            evidence: Vec::new(),
            risks: Vec::new(),
            recommendation: None,
            error: Some(error.into()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CouncilRunStatus {
    Completed,
    Failed,
    Cancelled,
    Uncertain,
}

impl CouncilRunStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Uncertain => "uncertain",
        }
    }

    fn root_work_item_status(self) -> WorkItemStatus {
        match self {
            Self::Completed => WorkItemStatus::Completed,
            Self::Cancelled => WorkItemStatus::Cancelled,
            Self::Failed | Self::Uncertain => WorkItemStatus::Blocked,
        }
    }
}

struct CouncilOutcome {
    status: CouncilRunStatus,
    message: Option<String>,
    seats: Vec<CouncilSeatResult>,
    synthesis: Option<String>,
}

impl CouncilOutcome {
    fn result(&self, run_id: &str, request: &CouncilRequest) -> Value {
        json!({
            "runID":run_id,
            "preset":request.preset,
            "question":request.question,
            "status":self.status.as_str(),
            "quorum":request.quorum,
            "seats":self.seats,
            "synthesis":self.synthesis,
        })
    }

    fn summary(&self, request: &CouncilRequest) -> String {
        let completed = self
            .seats
            .iter()
            .filter(|seat| seat.status == CouncilSeatStatus::Completed)
            .count();
        let mut lines = vec![match self.status {
            CouncilRunStatus::Completed => format!(
                "Council `{}` reached quorum {completed}/{} across {} seat(s).",
                request.preset,
                request.quorum,
                self.seats.len()
            ),
            CouncilRunStatus::Failed => format!(
                "Council `{}` failed with {completed}/{} valid seat(s): {}",
                request.preset,
                request.quorum,
                self.message.as_deref().unwrap_or("unknown failure")
            ),
            CouncilRunStatus::Cancelled => format!(
                "Council `{}` was cancelled after {completed} valid seat(s): {}",
                request.preset,
                self.message.as_deref().unwrap_or("cancelled")
            ),
            CouncilRunStatus::Uncertain => format!(
                "Council `{}` has an uncertain outcome after {completed} valid seat(s): {}",
                request.preset,
                self.message.as_deref().unwrap_or("uncertain")
            ),
        }];
        for seat in &self.seats {
            lines.push(format!(
                "\n### {} ({}) · {} · {} attempt(s)",
                seat.id,
                seat.agent,
                seat.status.as_str(),
                seat.attempts
            ));
            if let Some(verdict) = &seat.verdict {
                lines.push(format!("Verdict: {verdict}"));
            }
            if let Some(confidence) = seat.confidence {
                lines.push(format!("Confidence: {confidence:.2}"));
            }
            if let Some(recommendation) = &seat.recommendation {
                lines.push(format!("Recommendation: {recommendation}"));
            }
            if let Some(error) = &seat.error {
                lines.push(format!("Error: {error}"));
            }
        }
        if let Some(synthesis) = &self.synthesis {
            lines.push(format!("\n### Synthesis\n{synthesis}"));
        }
        lines.join("\n")
    }
}

#[async_trait]
trait CouncilSynthesizer: Send + Sync + 'static {
    async fn synthesize(&self, session_id: &str, payload: String) -> Result<String, String>;
}

struct ProviderCouncilSynthesizer {
    provider: Arc<dyn Provider>,
    agent: InternalAgent,
}

#[async_trait]
impl CouncilSynthesizer for ProviderCouncilSynthesizer {
    async fn synthesize(&self, session_id: &str, payload: String) -> Result<String, String> {
        complete_internal_text(
            session_id,
            "council-synth",
            self.provider.as_ref(),
            &self.agent,
            vec![
                Message::new(Role::System, self.agent.prompt.clone()),
                Message::new(Role::User, payload),
            ],
        )
        .await
    }
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
    council_synth: Arc<dyn CouncilSynthesizer>,
}

impl NativeWorkflowHost {
    pub(crate) fn new(
        database: Arc<zuno_db::pool::Pool>,
        child: ChildSessionHost,
        wake: Arc<dyn ParentReportWake>,
        supervisor: BackgroundJobSupervisor,
        council_provider: Arc<dyn Provider>,
        council_agent: InternalAgent,
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
            council_synth: Arc::new(ProviderCouncilSynthesizer {
                provider: council_provider,
                agent: council_agent,
            }),
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
        let max_parallel = request.max_parallel.max(1);
        let run_cancellation = cancellation.child_token();
        let mut tasks = JoinSet::new();

        loop {
            if cancellation.is_cancelled() {
                run_cancellation.cancel();
                if drain_cancelled(&mut tasks).await {
                    return WorkflowOutcome::Cancelled {
                        message: "cancelled by the parent turn".to_owned(),
                        completed: ordered_results(&results),
                    };
                }
                return WorkflowOutcome::Uncertain {
                    message:
                        "node tasks did not acknowledge cancellation before the safety timeout"
                            .to_owned(),
                    completed: ordered_results(&results),
                };
            }

            while tasks.len() < max_parallel {
                let next = pending.iter().copied().find(|index| {
                    request.nodes[*index]
                        .depends_on
                        .iter()
                        .all(|dependency| completed_ids.contains(dependency))
                });
                let Some(index) = next else {
                    break;
                };
                pending.remove(&index);
                if let Err(error) = self.transition_item(
                    &request.parent_session_id,
                    &mut items.nodes[index],
                    WorkItemStatus::InProgress,
                    None,
                    None,
                ) {
                    run_cancellation.cancel();
                    if !drain_cancelled(&mut tasks).await {
                        return WorkflowOutcome::Uncertain {
                            message: format!(
                                "workflow node `{}` could not enter running state ({error}) and active sibling cancellation could not be confirmed",
                                request.nodes[index].id
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
                        message: format!(
                            "workflow node `{}` could not enter running state: {error}",
                            request.nodes[index].id
                        ),
                        completed: ordered_results(&results),
                    };
                }
                items.nodes[index].started = Some(Instant::now());
                let node = request.nodes[index].clone();
                let runner = Arc::clone(&self.runner);
                let node_cancellation = run_cancellation.child_token();
                tasks.spawn(async move {
                    let id = node.id;
                    let agent = node.turn.agent.clone();
                    let started = Instant::now();
                    let result = runner.run(node.turn, node_cancellation).await;
                    (index, id, agent, elapsed_millis(started), result)
                });
            }

            if tasks.is_empty() {
                if pending.is_empty() {
                    if cancellation.is_cancelled() {
                        return WorkflowOutcome::Cancelled {
                            message: "cancelled by the parent turn".to_owned(),
                            completed: ordered_results(&results),
                        };
                    }
                    return WorkflowOutcome::Completed(ordered_results(&results));
                }
                return WorkflowOutcome::Failed {
                    message: "no runnable node remains; dependencies did not reach a successful terminal state"
                        .to_owned(),
                    completed: ordered_results(&results),
                };
            }

            let joined = tokio::select! {
                biased;
                () = cancellation.cancelled() => {
                    run_cancellation.cancel();
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
                continue;
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
                            run_cancellation.cancel();
                            let _drained = drain_cancelled(&mut tasks).await;
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
                        run_cancellation.cancel();
                        let _drained = drain_cancelled(&mut tasks).await;
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
                    run_cancellation.cancel();
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
                    run_cancellation.cancel();
                    let _drained = drain_cancelled(&mut tasks).await;
                    return WorkflowOutcome::Uncertain {
                        message: format!("a workflow node task was lost: {error}"),
                        completed: ordered_results(&results),
                    };
                }
            }
        }
    }

    async fn execute_council_managed(
        &self,
        request: &CouncilRequest,
        cancellation: CancellationToken,
        items: &mut WorkflowItems,
    ) -> CouncilOutcome {
        let mut outcome = self.execute_council(request, cancellation, items).await;
        if let Err(error) = self.finalize_council_items(request, items, &outcome) {
            outcome.status = CouncilRunStatus::Uncertain;
            outcome.message = Some(match outcome.message.take() {
                Some(message) => {
                    format!("{message}; durable WorkItem settlement failed: {error}")
                }
                None => format!("durable WorkItem settlement failed: {error}"),
            });
        }
        outcome
    }

    fn finalize_council_items(
        &self,
        request: &CouncilRequest,
        items: &mut WorkflowItems,
        outcome: &CouncilOutcome,
    ) -> Result<(), String> {
        let mut errors = Vec::new();
        for (index, item) in items.nodes.iter_mut().enumerate() {
            if work_item_terminal(item.status) {
                continue;
            }
            let status = outcome.seats.get(index).map_or_else(
                || outcome.status.root_work_item_status(),
                |seat| seat.status.work_item_status(),
            );
            let elapsed = item.started.map(elapsed_millis).unwrap_or_default();
            if let Err(error) = self.transition_item(
                &request.parent_session_id,
                item,
                status,
                Some(elapsed),
                item.tokens_used,
            ) {
                errors.push(error);
            }
        }
        let root_tokens = items
            .nodes
            .iter()
            .map(|item| item.tokens_used)
            .collect::<Option<Vec<_>>>()
            .map(|tokens| tokens.into_iter().fold(0_i64, i64::saturating_add));
        if !work_item_terminal(items.root.status)
            && let Err(error) = self.transition_item(
                &request.parent_session_id,
                &mut items.root,
                outcome.status.root_work_item_status(),
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

    async fn execute_council(
        &self,
        request: &CouncilRequest,
        cancellation: CancellationToken,
        items: &mut WorkflowItems,
    ) -> CouncilOutcome {
        let started = Instant::now();
        let total_deadline = started
            .checked_add(request.deadline)
            .unwrap_or_else(Instant::now);
        let seat_deadline = started
            .checked_add(request.deadline.saturating_sub(request.synthesis_timeout))
            .unwrap_or_else(Instant::now);
        let execution_cancellation = cancellation.child_token();
        let mut tasks = JoinSet::new();
        let mut next = 0_usize;
        let mut results = vec![None; request.seats.len()];

        while next < request.seats.len() || !tasks.is_empty() {
            while next < request.seats.len() && tasks.len() < request.max_parallel.max(1) {
                if cancellation.is_cancelled() {
                    execution_cancellation.cancel();
                    let confirmed = drain_council_cancelled(&mut tasks).await;
                    return interrupted_council_outcome(
                        request,
                        &results,
                        confirmed,
                        "cancelled before the next Council seat was admitted",
                    );
                }
                if let Err(error) = self.transition_item(
                    &request.parent_session_id,
                    &mut items.nodes[next],
                    WorkItemStatus::InProgress,
                    None,
                    None,
                ) {
                    execution_cancellation.cancel();
                    let confirmed = drain_council_cancelled(&mut tasks).await;
                    let message = if confirmed {
                        format!(
                            "Council seat `{}` could not enter running state: {error}",
                            request.seats[next].id
                        )
                    } else {
                        format!(
                            "Council seat `{}` could not enter running state ({error}) and active seat cancellation was not confirmed",
                            request.seats[next].id
                        )
                    };
                    return CouncilOutcome {
                        status: CouncilRunStatus::Uncertain,
                        message: Some(message.clone()),
                        seats: complete_council_results(
                            request,
                            &results,
                            CouncilSeatStatus::Uncertain,
                            &message,
                        ),
                        synthesis: None,
                    };
                }
                items.nodes[next].started = Some(Instant::now());
                let runner = Arc::clone(&self.runner);
                let seat = request.seats[next].clone();
                let seat_cancellation = execution_cancellation.child_token();
                let max_retries = request.max_retries;
                let output_limit = request.seat_output_bytes;
                let index = next;
                tasks.spawn(async move {
                    let seat_started = Instant::now();
                    let result = run_council_seat(
                        runner,
                        seat,
                        seat_cancellation,
                        seat_deadline,
                        max_retries,
                        output_limit,
                    )
                    .await;
                    (index, elapsed_millis(seat_started), result)
                });
                next = next.saturating_add(1);
            }

            if tasks.is_empty() {
                break;
            }
            let joined = tokio::select! {
                // Parent cancellation is authoritative when a seat completion and the
                // interrupt become ready in the same scheduler tick.
                biased;
                () = cancellation.cancelled() => {
                    execution_cancellation.cancel();
                    let confirmed = drain_council_cancelled(&mut tasks).await;
                    return interrupted_council_outcome(
                        request,
                        &results,
                        confirmed,
                        "cancelled by the parent turn",
                    );
                }
                joined = tasks.join_next() => joined,
            };
            let Some(joined) = joined else {
                break;
            };
            let (index, elapsed_ms, mut result) = match joined {
                Ok(joined) => joined,
                Err(error) => {
                    execution_cancellation.cancel();
                    let _confirmed = drain_council_cancelled(&mut tasks).await;
                    return CouncilOutcome {
                        status: CouncilRunStatus::Uncertain,
                        message: Some(format!("a Council seat task was lost: {error}")),
                        seats: complete_council_results(
                            request,
                            &results,
                            CouncilSeatStatus::Uncertain,
                            "seat task was lost before a terminal result",
                        ),
                        synthesis: None,
                    };
                }
            };
            let tokens = match result.session_id.as_deref() {
                Some(session_id) => match self.child_tokens(session_id) {
                    Ok(tokens) => tokens,
                    Err(error) => {
                        result.status = CouncilSeatStatus::Uncertain;
                        result.error = Some(format!(
                            "Council seat usage could not be reconciled after completion: {error}"
                        ));
                        None
                    }
                },
                None => None,
            };
            if let Err(error) = self.transition_item(
                &request.parent_session_id,
                &mut items.nodes[index],
                result.status.work_item_status(),
                Some(elapsed_ms),
                tokens,
            ) {
                execution_cancellation.cancel();
                let confirmed = drain_council_cancelled(&mut tasks).await;
                results[index] = Some(result);
                return CouncilOutcome {
                    status: CouncilRunStatus::Uncertain,
                    message: Some(if confirmed {
                        format!(
                            "Council seat `{}` terminated but its WorkItem could not be settled: {error}",
                            request.seats[index].id
                        )
                    } else {
                        format!(
                            "Council seat `{}` WorkItem settlement failed ({error}) and sibling cancellation was not confirmed",
                            request.seats[index].id
                        )
                    }),
                    seats: complete_council_results(
                        request,
                        &results,
                        CouncilSeatStatus::Uncertain,
                        "Council execution stopped after durable state loss",
                    ),
                    synthesis: None,
                };
            }
            items.nodes[index].tokens_used = tokens;
            results[index] = Some(result);
        }

        let seats = complete_council_results(
            request,
            &results,
            CouncilSeatStatus::Uncertain,
            "seat ended without a terminal result",
        );
        if cancellation.is_cancelled()
            || seats
                .iter()
                .any(|seat| seat.status == CouncilSeatStatus::Cancelled)
        {
            return CouncilOutcome {
                status: CouncilRunStatus::Cancelled,
                message: Some("cancelled by the parent turn".to_owned()),
                seats,
                synthesis: None,
            };
        }
        if seats
            .iter()
            .any(|seat| seat.status == CouncilSeatStatus::Uncertain)
        {
            return CouncilOutcome {
                status: CouncilRunStatus::Uncertain,
                message: Some("one or more Council seats have an uncertain outcome".to_owned()),
                seats,
                synthesis: None,
            };
        }
        let valid = seats
            .iter()
            .filter(|seat| seat.status == CouncilSeatStatus::Completed)
            .count();
        if valid < request.quorum {
            return CouncilOutcome {
                status: CouncilRunStatus::Failed,
                message: Some(format!(
                    "quorum was not reached: {valid} valid seat(s), {} required",
                    request.quorum
                )),
                seats,
                synthesis: None,
            };
        }

        let payload = match synthesis_payload(request, &seats) {
            Ok(payload) => payload,
            Err(error) => {
                return CouncilOutcome {
                    status: CouncilRunStatus::Failed,
                    message: Some(error),
                    seats,
                    synthesis: None,
                };
            }
        };
        let remaining = request
            .synthesis_timeout
            .min(total_deadline.saturating_duration_since(Instant::now()));
        if remaining.is_zero() {
            return CouncilOutcome {
                status: CouncilRunStatus::Failed,
                message: Some("Council synthesis budget expired before synthesis".to_owned()),
                seats,
                synthesis: None,
            };
        }
        let synthesis = tokio::select! {
            // A completed synthesis must not revive a Council cancelled in the same tick.
            biased;
            () = cancellation.cancelled() => {
                return CouncilOutcome {
                    status: CouncilRunStatus::Cancelled,
                    message: Some("cancelled before Council synthesis completed".to_owned()),
                    seats,
                    synthesis: None,
                };
            }
            result = timeout(
                remaining,
                self.council_synth.synthesize(&request.parent_session_id, payload),
            ) => result,
        };
        let synthesis = match synthesis {
            Ok(Ok(text)) if !text.trim().is_empty() => text,
            Ok(Ok(_)) => {
                return CouncilOutcome {
                    status: CouncilRunStatus::Failed,
                    message: Some("Council synthesizer returned no text".to_owned()),
                    seats,
                    synthesis: None,
                };
            }
            Ok(Err(error)) => {
                return CouncilOutcome {
                    status: CouncilRunStatus::Failed,
                    message: Some(format!("Council synthesis failed: {error}")),
                    seats,
                    synthesis: None,
                };
            }
            Err(_) => {
                return CouncilOutcome {
                    status: CouncilRunStatus::Failed,
                    message: Some("Council synthesis budget expired during synthesis".to_owned()),
                    seats,
                    synthesis: None,
                };
            }
        };
        CouncilOutcome {
            status: CouncilRunStatus::Completed,
            message: None,
            seats,
            synthesis: Some(synthesis),
        }
    }

    async fn settle_council(
        &self,
        request: &CouncilRequest,
        run_id: &str,
        job_id: &str,
        outcome: &CouncilOutcome,
    ) -> Result<String, String> {
        let completed = zuno_db::message::now_millis();
        let summary = outcome.summary(request);
        let report = report_for_council(
            request,
            job_id,
            run_id,
            outcome.status.as_str(),
            &summary,
            completed,
        );
        let result = outcome.result(run_id, request);
        let settlement = match outcome.status {
            CouncilRunStatus::Completed => JobSettlement::completed(result, completed, report),
            CouncilRunStatus::Failed => JobSettlement::failed(
                outcome.message.as_deref().unwrap_or("Council failed"),
                completed,
                report,
            )
            .with_result(result),
            CouncilRunStatus::Cancelled => JobSettlement::cancelled(
                outcome.message.as_deref().unwrap_or("Council cancelled"),
                completed,
                report,
            )
            .with_result(result),
            CouncilRunStatus::Uncertain => JobSettlement::uncertain(
                outcome
                    .message
                    .as_deref()
                    .unwrap_or("Council outcome uncertain"),
                completed,
                report,
            )
            .with_result(result),
        };
        let settled = self.jobs.settle(job_id, settlement).map_err(to_string)?;
        self.changes.changed();
        if let Some(report) = settled.report {
            self.wake.wake(report).await?;
        }
        Ok(summary)
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

#[async_trait]
impl CouncilHost for NativeWorkflowHost {
    async fn dispatch(
        &self,
        request: CouncilRequest,
        cancellation: CancellationToken,
    ) -> Result<CouncilTurn, String> {
        if request.seats.is_empty()
            || request.quorum == 0
            || request.quorum > request.seats.len()
            || request.max_parallel == 0
            || request.max_parallel > request.seats.len()
            || request.deadline.is_zero()
            || request.synthesis_timeout.is_zero()
            || request.synthesis_timeout >= request.deadline
            || request.seat_output_bytes == 0
            || request.synthesis_input_bytes == 0
        {
            return Err("Council request has invalid seats, quorum, or bounds".to_owned());
        }
        let run_id = super::turn::prefixed_id("run");
        let job_id = super::turn::prefixed_id("job");
        let workflow_name = format!("council:{}", request.preset);
        let workflow_request = workflow_request_for_council(&request, &workflow_name);
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
                    JobSubject::workflow(run_id.clone(), workflow_name),
                    delivery,
                    zuno_db::message::now_millis(),
                )
                .with_orchestration_snapshot(request.parent_attempt.as_deref().cloned()),
            )
            .map_err(to_string)?;
        let mut work_items = match self.admit_work_items(&workflow_request, &run_id) {
            Ok(items) => items,
            Err(error) => {
                let message = format!(
                    "Council `{}` was not admitted because its durable WorkItems could not be created: {error}",
                    request.preset
                );
                let result = json!({
                    "runID":run_id,
                    "preset":request.preset,
                    "status":"failed",
                    "seats":[],
                });
                let _settled = self.jobs.settle(
                    &job_id,
                    JobSettlement::failed(message.clone(), zuno_db::message::now_millis(), None)
                        .with_result(result),
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
                        .execute_council_managed(&request, task_cancellation, &mut work_items)
                        .await;
                    if let Err(error) = host
                        .settle_council(&request, &background_run_id, &background_job_id, &outcome)
                        .await
                    {
                        tracing::error!(
                            job_id = %background_job_id,
                            %error,
                            "Council job settlement failed"
                        );
                    }
                },
            );
            return Ok(CouncilTurn {
                run_id,
                job_id: Some(job_id),
                output: "Council started. Its frozen seats are running under the configured concurrency, quorum, retry, and deadline bounds."
                    .to_owned(),
            });
        }

        let outcome = self
            .execute_council_managed(&request, cancellation, &mut work_items)
            .await;
        let summary = self
            .settle_council(&request, &run_id, &job_id, &outcome)
            .await?;
        if outcome.status == CouncilRunStatus::Completed {
            Ok(CouncilTurn {
                run_id,
                job_id: None,
                output: summary,
            })
        } else {
            Err(summary)
        }
    }
}

fn workflow_request_for_council(request: &CouncilRequest, workflow: &str) -> WorkflowRequest {
    WorkflowRequest {
        parent_session_id: request.parent_session_id.clone(),
        parent_attempt: request.parent_attempt.clone(),
        workflow: workflow.to_owned(),
        description: request.description.clone(),
        nodes: request
            .seats
            .iter()
            .map(|seat| zuno_tools::workflow::WorkflowNodeRequest {
                id: seat.id.clone(),
                depends_on: Vec::new(),
                turn: seat.turn.clone(),
            })
            .collect(),
        max_parallel: request.max_parallel,
        background: request.background,
        report_delivery: request.report_delivery,
    }
}

async fn run_council_seat(
    runner: Arc<dyn WorkflowNodeRunner>,
    seat: CouncilSeatRequest,
    cancellation: CancellationToken,
    seat_deadline: Instant,
    max_retries: usize,
    output_limit: usize,
) -> CouncilSeatResult {
    let attempts_allowed = max_retries.saturating_add(1);
    let mut last_status = CouncilSeatStatus::Failed;
    let mut last_error = "Council seat did not start".to_owned();
    let mut last_session_id = None;
    for attempt in 1..=attempts_allowed {
        if cancellation.is_cancelled() {
            return CouncilSeatResult::terminal(
                &seat,
                CouncilSeatStatus::Cancelled,
                attempt.saturating_sub(1),
                last_session_id,
                "cancelled before the next seat attempt",
            );
        }
        let remaining = seat_deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return CouncilSeatResult::terminal(
                &seat,
                CouncilSeatStatus::TimedOut,
                attempt.saturating_sub(1),
                last_session_id,
                "Council seat-phase deadline expired before the next attempt",
            );
        }
        let attempt_cancellation = cancellation.child_token();
        let future = runner.run(seat.turn.clone(), attempt_cancellation.clone());
        tokio::pin!(future);
        let result = tokio::select! {
            // The runner commonly returns an error as it acknowledges cancellation. Give
            // the parent signal precedence so that acknowledgement remains `cancelled`
            // instead of being misclassified as an ordinary seat failure.
            biased;
            () = cancellation.cancelled() => {
                attempt_cancellation.cancel();
                return if timeout(CANCEL_DRAIN_TIMEOUT, &mut future).await.is_ok() {
                    CouncilSeatResult::terminal(
                        &seat,
                        CouncilSeatStatus::Cancelled,
                        attempt,
                        last_session_id,
                        "cancelled by the parent Council",
                    )
                } else {
                    CouncilSeatResult::terminal(
                        &seat,
                        CouncilSeatStatus::Uncertain,
                        attempt,
                        last_session_id,
                        "seat did not acknowledge cancellation before the safety timeout",
                    )
                };
            }
            result = timeout(remaining, &mut future) => result,
        };
        let turn = match result {
            Ok(Ok(turn)) => turn,
            Ok(Err(error)) => {
                last_status = CouncilSeatStatus::Failed;
                last_error = error;
                continue;
            }
            Err(_) => {
                attempt_cancellation.cancel();
                return if timeout(CANCEL_DRAIN_TIMEOUT, &mut future).await.is_ok() {
                    CouncilSeatResult::terminal(
                        &seat,
                        CouncilSeatStatus::TimedOut,
                        attempt,
                        last_session_id,
                        "Council seat exceeded the seat-phase deadline",
                    )
                } else {
                    CouncilSeatResult::terminal(
                        &seat,
                        CouncilSeatStatus::Uncertain,
                        attempt,
                        last_session_id,
                        "timed-out seat did not acknowledge cancellation before the safety timeout",
                    )
                };
            }
        };
        last_session_id = Some(turn.session_id.clone());
        match parse_council_answer(&turn.output, output_limit) {
            Ok(answer) => {
                return CouncilSeatResult::completed(&seat, attempt, turn.session_id, answer);
            }
            Err(error) => {
                last_status = CouncilSeatStatus::Invalid;
                last_error = error;
            }
        }
    }
    CouncilSeatResult::terminal(
        &seat,
        last_status,
        attempts_allowed,
        last_session_id,
        last_error,
    )
}

fn parse_council_answer(output: &str, output_limit: usize) -> Result<CouncilSeatAnswer, String> {
    if output.len() > output_limit {
        return Err(format!(
            "seat response exceeded the {output_limit}-byte output bound"
        ));
    }
    let answer: CouncilSeatAnswer = serde_json::from_str(output.trim())
        .map_err(|error| format!("seat returned malformed structured output: {error}"))?;
    validate_council_text("verdict", &answer.verdict)?;
    validate_council_text("recommendation", &answer.recommendation)?;
    if !answer.confidence.is_finite() || !(0.0..=1.0).contains(&answer.confidence) {
        return Err("seat confidence must be a finite number from 0 to 1".to_owned());
    }
    for (name, values) in [("evidence", &answer.evidence), ("risks", &answer.risks)] {
        if values.len() > MAX_COUNCIL_LIST_ITEMS {
            return Err(format!(
                "seat {name} exceeds the {MAX_COUNCIL_LIST_ITEMS}-item bound"
            ));
        }
        for value in values {
            validate_council_text(name, value)?;
        }
    }
    Ok(answer)
}

fn validate_council_text(field: &str, value: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err(format!("seat `{field}` must not be empty"));
    }
    if value.len() > MAX_COUNCIL_FIELD_BYTES {
        return Err(format!(
            "seat `{field}` exceeds the {MAX_COUNCIL_FIELD_BYTES}-byte field bound"
        ));
    }
    Ok(())
}

fn complete_council_results(
    request: &CouncilRequest,
    results: &[Option<CouncilSeatResult>],
    fallback_status: CouncilSeatStatus,
    fallback_error: &str,
) -> Vec<CouncilSeatResult> {
    request
        .seats
        .iter()
        .enumerate()
        .map(|(index, seat)| {
            results[index].clone().unwrap_or_else(|| {
                CouncilSeatResult::terminal(seat, fallback_status, 0, None, fallback_error)
            })
        })
        .collect()
}

fn interrupted_council_outcome(
    request: &CouncilRequest,
    results: &[Option<CouncilSeatResult>],
    cancellation_confirmed: bool,
    message: &str,
) -> CouncilOutcome {
    let (status, seat_status) = if cancellation_confirmed {
        (CouncilRunStatus::Cancelled, CouncilSeatStatus::Cancelled)
    } else {
        (CouncilRunStatus::Uncertain, CouncilSeatStatus::Uncertain)
    };
    CouncilOutcome {
        status,
        message: Some(if cancellation_confirmed {
            message.to_owned()
        } else {
            format!("{message}; running seats did not acknowledge cancellation")
        }),
        seats: complete_council_results(request, results, seat_status, message),
        synthesis: None,
    }
}

fn synthesis_payload(
    request: &CouncilRequest,
    seats: &[CouncilSeatResult],
) -> Result<String, String> {
    let payload = serde_json::to_string(&json!({
        "question":request.question,
        "quorum":request.quorum,
        "seats":seats,
    }))
    .map_err(to_string)?;
    if payload.len() > request.synthesis_input_bytes {
        return Err(format!(
            "structured Council synthesis input is {} bytes, exceeding the configured {}-byte bound",
            payload.len(),
            request.synthesis_input_bytes
        ));
    }
    Ok(payload)
}

async fn drain_council_cancelled(tasks: &mut JoinSet<CouncilJoin>) -> bool {
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

fn report_for_council(
    request: &CouncilRequest,
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
                "kind":"councilReport",
                "jobID":job_id,
                "runID":run_id,
                "preset":request.preset,
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
