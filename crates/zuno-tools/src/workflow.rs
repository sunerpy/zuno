//! Instantiation of validated, configuration-owned multi-agent workflows.
//!
//! The model can select a template and supply its task, but it cannot manufacture a
//! graph, change dependencies, raise concurrency, or select agents outside the
//! configured template. Model and reasoning resolution reuse [`crate::task::TaskTool`]
//! so direct delegation and workflow nodes cannot disagree about provider policy.

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use zuno_error::ToolError;
use zuno_orchestration::{AttemptSnapshot, WorkflowTemplateDescriptor, sha256_json};
use zuno_tool::{
    PermissionAsk, ToolConcurrencyPolicy, ToolContext, ToolOutput, ToolReplayPolicy, ToolUiIntent,
    TypedTool,
};

use crate::task::{ChildTurnRequest, DelegationModelRequest, ReportDelivery, TaskTool};

/// Stable model-facing id for configured workflow execution.
pub const WIRE_ID: &str = "workflow";
/// Workflow execution shares the bounded-delegation permission domain.
pub const PERMISSION_KEY: &str = "task";

/// Arguments for one immutable workflow-template invocation.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkflowParams {
    /// Configured workflow template name.
    pub workflow: String,
    /// Root task supplied unchanged to every node, followed by its configured node prompt.
    pub prompt: String,
    /// Short label shown in durable jobs and clients.
    #[serde(default)]
    pub description: Option<String>,
    /// Run asynchronously and return a durable job id immediately.
    #[serde(default)]
    pub background: Option<bool>,
    /// How a background terminal report reaches the parent session.
    #[serde(default, rename = "reportDelivery")]
    pub report_delivery: Option<ReportDelivery>,
}

/// One fully resolved node admitted to the runtime scheduler.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkflowNodeRequest {
    /// Stable template-local node id.
    pub id: String,
    /// Stable dependencies in template declaration order.
    pub depends_on: Vec<String>,
    /// Resolved child turn; the host cannot choose a different model or effort.
    pub turn: ChildTurnRequest,
}

/// A validated workflow run admitted by the tool layer.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkflowRequest {
    /// Parent session that owns the workflow and its durable job.
    pub parent_session_id: String,
    /// Immutable parent Attempt which admitted this workflow.
    pub parent_attempt: Option<Arc<AttemptSnapshot>>,
    /// Immutable configured template name.
    pub workflow: String,
    /// Optional human-readable label.
    pub description: Option<String>,
    /// Nodes in stable template declaration order.
    pub nodes: Vec<WorkflowNodeRequest>,
    /// Configuration-owned concurrency bound.
    pub max_parallel: usize,
    /// Whether the tool returns before the scheduler settles.
    pub background: bool,
    /// Durable report behavior for a background invocation.
    pub report_delivery: ReportDelivery,
}

/// Result returned by the workflow runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowTurn {
    /// One invocation id, distinct from the durable job id.
    pub run_id: String,
    /// Durable job handle for background execution only.
    pub job_id: Option<String>,
    /// Final summary or a running notice.
    pub output: String,
}

/// Workflow scheduling effects supplied by the CLI composition root.
#[async_trait]
pub trait WorkflowHost: Send + Sync + 'static {
    /// Run or enqueue one already-expanded workflow template.
    async fn dispatch(
        &self,
        request: WorkflowRequest,
        cancellation: CancellationToken,
    ) -> Result<WorkflowTurn, String>;
}

/// A model-facing tool that can only instantiate known workflow templates.
pub struct WorkflowTool {
    templates: Vec<WorkflowTemplateDescriptor>,
    task: TaskTool,
    host: Arc<dyn WorkflowHost>,
    description: String,
}

impl WorkflowTool {
    /// Bind validated templates to the shared delegation planner and runtime host.
    pub fn new(
        templates: impl IntoIterator<Item = WorkflowTemplateDescriptor>,
        task: TaskTool,
        host: Arc<dyn WorkflowHost>,
    ) -> Result<Self, String> {
        let templates = templates.into_iter().collect::<Vec<_>>();
        if templates.is_empty() {
            return Err("workflow tool requires at least one configured template".to_owned());
        }
        let targets = task.targets();
        for template in &templates {
            let name = &template.name;
            if name.trim().is_empty()
                || template.source_id.trim().is_empty()
                || template.nodes.is_empty()
                || template.max_parallel == 0
                || template.max_parallel > template.max_agents
                || template.nodes.len() > template.max_agents
            {
                return Err(format!(
                    "workflow descriptor `{name}` has invalid identity, graph, or bounds"
                ));
            }
            for node in &template.nodes {
                if !targets.iter().any(|target| target == &node.agent) {
                    return Err(format!(
                        "workflows.{name}.nodes.{} targets agent `{}` outside the current agent's delegate allowlist ({})",
                        node.id,
                        node.agent,
                        targets.join(", ")
                    ));
                }
            }
        }
        let available = templates
            .iter()
            .map(|template| template.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        Ok(Self {
            templates,
            task,
            host,
            description: format!(
                "Instantiate one validated multi-agent workflow template. The graph, agents, dependencies, and concurrency are configuration-owned and cannot be supplied by the caller. Available templates: {available}."
            ),
        })
    }

    fn template(&self, name: &str) -> Option<&WorkflowTemplateDescriptor> {
        self.templates.iter().find(|template| template.name == name)
    }

    fn expand(
        &self,
        template: &WorkflowTemplateDescriptor,
        params: &WorkflowParams,
        parent_session_id: &str,
        parent_attempt: Option<&Arc<AttemptSnapshot>>,
    ) -> Vec<WorkflowNodeRequest> {
        template
            .nodes
            .iter()
            .map(|node| {
                let prompt = node.prompt.as_ref().map_or_else(
                    || params.prompt.clone(),
                    |instruction| {
                        format!(
                            "{}\n\nWorkflow node `{}` instruction:\n{}",
                            params.prompt, node.id, instruction
                        )
                    },
                );
                let description = node
                    .description
                    .clone()
                    .or_else(|| Some(format!("{} / {}", params.workflow, node.id)));
                let plan = self
                    .task
                    .plan(&node.agent, None, &DelegationModelRequest::default());
                WorkflowNodeRequest {
                    id: node.id.clone(),
                    depends_on: node.depends_on.clone(),
                    turn: ChildTurnRequest {
                        parent_session_id: parent_session_id.to_owned(),
                        parent_attempt: parent_attempt.map(Arc::clone),
                        workflow: Some(template.name.clone()),
                        workflow_node: Some(node.id.clone()),
                        resume_session_id: None,
                        logical_key: format!(
                            "workflow:v1:{}",
                            sha256_json(&json!({
                                "workflow": template.name,
                                "node": node.id,
                                "agent": node.agent,
                                "prompt": prompt,
                            }))
                        ),
                        agent: node.agent.clone(),
                        description,
                        prompt,
                        model: plan.model,
                        effort: plan.effort,
                        provider_options: plan.provider_options,
                        background: false,
                        report_delivery: ReportDelivery::Quiet,
                    },
                }
            })
            .collect()
    }
}

#[async_trait]
impl TypedTool for WorkflowTool {
    type Params = WorkflowParams;

    fn id(&self) -> &str {
        WIRE_ID
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn replay_policy(&self) -> ToolReplayPolicy {
        ToolReplayPolicy::Never
    }

    fn concurrency_policy(&self) -> ToolConcurrencyPolicy {
        ToolConcurrencyPolicy::IsolatedBackground
    }

    fn ui_intent(&self) -> ToolUiIntent {
        ToolUiIntent::Subagent
    }

    async fn run(&self, params: WorkflowParams, ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        let name = params.workflow.trim();
        if name.is_empty() {
            return Err(invalid("`workflow` must not be empty"));
        }
        if params.prompt.trim().is_empty() {
            return Err(invalid("`prompt` must not be empty"));
        }
        let background = params.background.unwrap_or(false);
        if !background && params.report_delivery.is_some() {
            return Err(invalid(
                "`reportDelivery` requires `background: true`; remove it or run in background",
            ));
        }
        let template = self.template(name).ok_or_else(|| {
            invalid(&format!(
                "unknown workflow `{name}`; choose one of: {}",
                self.templates
                    .iter()
                    .map(|template| template.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })?;
        self.task.guard_depth(&ctx).await?;

        let mut metadata = Map::new();
        metadata.insert("workflow".to_owned(), Value::String(name.to_owned()));
        metadata.insert(
            "nodes".to_owned(),
            Value::Number(serde_json::Number::from(template.nodes.len() as u64)),
        );
        if let Some(description) = &params.description {
            metadata.insert("description".to_owned(), Value::String(description.clone()));
        }
        ctx.ask(
            WIRE_ID,
            PermissionAsk {
                permission: PERMISSION_KEY.to_owned(),
                patterns: vec![format!("workflow:{name}")],
                metadata,
                always: vec![format!("workflow:{name}")],
                ..PermissionAsk::default()
            },
        )
        .await?;

        let parent_session_id = ctx.session_id.clone();
        let parent_attempt = ctx.orchestration_snapshot().cloned();
        let request = WorkflowRequest {
            parent_session_id: parent_session_id.clone(),
            parent_attempt: parent_attempt.clone(),
            workflow: name.to_owned(),
            description: params.description.clone(),
            nodes: self.expand(
                template,
                &params,
                &parent_session_id,
                parent_attempt.as_ref(),
            ),
            max_parallel: template.max_parallel,
            background,
            report_delivery: params.report_delivery.unwrap_or_default(),
        };
        let cancellation = CancellationToken::new();
        let dispatch = self.host.dispatch(request, cancellation.clone());
        tokio::pin!(dispatch);
        let turn = if background {
            dispatch.await
        } else {
            tokio::select! {
                result = &mut dispatch => result,
                () = ctx.interrupt.notified() => {
                    cancellation.cancel();
                    dispatch.await
                }
            }
        }
        .map_err(failed)?;
        if background && turn.job_id.is_none() {
            return Err(failed(
                "a background workflow dispatch did not return a durable job id".to_owned(),
            ));
        }
        Ok(render(&params, &turn))
    }
}

fn render(params: &WorkflowParams, turn: &WorkflowTurn) -> ToolOutput {
    let state = if turn.job_id.is_some() {
        "running"
    } else {
        "completed"
    };
    let delivery = match params.report_delivery.unwrap_or_default() {
        ReportDelivery::NextStep => "nextStep",
        ReportDelivery::Quiet => "quiet",
    };
    let mut lines = vec![match turn.job_id.as_deref() {
        Some(job) => format!(
            "<workflow name=\"{}\" run=\"{}\" job=\"{job}\" state=\"{state}\" reportDelivery=\"{delivery}\">",
            params.workflow, turn.run_id
        ),
        None => format!(
            "<workflow name=\"{}\" run=\"{}\" state=\"{state}\">",
            params.workflow, turn.run_id
        ),
    }];
    if let Some(description) = &params.description {
        lines.push(format!("<summary>{description}</summary>"));
    }
    lines.push("<workflow_result>".to_owned());
    lines.push(turn.output.clone());
    lines.push("</workflow_result>".to_owned());
    lines.push("</workflow>".to_owned());
    ToolOutput::text(
        params
            .description
            .clone()
            .unwrap_or_else(|| format!("{} workflow", params.workflow)),
        lines.join("\n"),
    )
    .with_metadata(
        "subagent",
        json!({
            "kind":"workflow",
            "workflow":params.workflow,
            "runID":turn.run_id,
            "jobID":turn.job_id,
            "state":state,
            "reportDelivery":delivery,
            "description":params.description,
            "result":turn.output,
        }),
    )
}

fn invalid(message: &str) -> ToolError {
    ToolError::InvalidArgs {
        tool: WIRE_ID.to_owned(),
        source: Box::new(std::io::Error::other(message.to_owned())),
    }
}

fn failed(message: String) -> ToolError {
    ToolError::Failed {
        tool: WIRE_ID.to_owned(),
        source: Box::new(std::io::Error::other(message)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use zuno_orchestration::WorkflowNodeDescriptor;
    use zuno_tool::{AllowAll, NeverInterrupted};

    #[derive(Default)]
    struct RecordingWorkflowHost {
        requests: Mutex<Vec<WorkflowRequest>>,
    }

    #[async_trait]
    impl WorkflowHost for RecordingWorkflowHost {
        async fn dispatch(
            &self,
            request: WorkflowRequest,
            _cancellation: CancellationToken,
        ) -> Result<WorkflowTurn, String> {
            self.requests
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(request);
            Ok(WorkflowTurn {
                run_id: "run_test".to_owned(),
                job_id: None,
                output: "done".to_owned(),
            })
        }
    }

    fn template() -> WorkflowTemplateDescriptor {
        WorkflowTemplateDescriptor {
            name: "release".to_owned(),
            source_id: "test://workflow/release".to_owned(),
            max_parallel: 2,
            max_agents: 2,
            nodes: vec![
                WorkflowNodeDescriptor {
                    id: "scan".to_owned(),
                    agent: "explore".to_owned(),
                    prompt: Some("Collect evidence.".to_owned()),
                    description: None,
                    depends_on: Vec::new(),
                },
                WorkflowNodeDescriptor {
                    id: "implement".to_owned(),
                    agent: "worker".to_owned(),
                    prompt: None,
                    description: None,
                    depends_on: vec!["scan".to_owned()],
                },
            ],
        }
    }

    fn orchestration_snapshot() -> Arc<AttemptSnapshot> {
        Arc::new(
            serde_json::from_value(json!({
                "schemaVersion": 4,
                "turnId": "turn-parent",
                "step": 1,
                "capability": {
                    "schemaVersion": 4,
                    "pack": {"id":"test","version":"1","upstreamRevision":"test"},
                    "extensionRevision": 0,
                    "permissionPolicySha256": "policy",
                    "sandbox": {
                        "mode": "workspace-write",
                        "network": "deny",
                        "writableRoots": [],
                        "protectedPaths": []
                    },
                    "profiles": [], "presets": [], "councils": [], "workflows": [], "skills": []
                },
                "owner": {
                    "sessionId":"ses_parent", "parentSessionId":null, "parentAttempt":null,
                    "workflow":null, "workflowNode":null
                },
                "agent": {
                    "name":"build", "sourceId":"test://build",
                    "definitionSha256":"definition", "permissionSha256":"permission",
                    "promptPolicySha256":"prompt"
                },
                "model": {
                    "providerId":"fake", "modelId":"fake-model", "wireModelId":"fake-model",
                    "surface":"responses", "reasoningSha256":"reasoning", "preset":null
                },
                "selectedSkills": [],
                "prompt": {"eventId":"evt-parent","assemblySha256":"assembly","actualSha256":"actual"},
                "tools": []
            }))
            .expect("test orchestration snapshot"),
        )
    }

    #[tokio::test]
    async fn expands_only_the_configured_dag_with_resolved_child_turns() {
        let host = Arc::new(RecordingWorkflowHost::default());
        let snapshot = orchestration_snapshot();
        let task = TaskTool::new(
            Arc::new(crate::task::RecordingHost::new()),
            Arc::new(crate::task::NoProviders),
        )
        .with_targets(
            crate::task::DelegationTargets::new(["explore".to_owned(), "worker".to_owned()])
                .expect("targets"),
        );
        let tool = WorkflowTool::new([template()], task, host.clone()).expect("workflow tool");
        let output = tool
            .run(
                WorkflowParams {
                    workflow: "release".to_owned(),
                    prompt: "Prepare the repository.".to_owned(),
                    description: None,
                    background: None,
                    report_delivery: None,
                },
                ToolContext::new(
                    "ses_parent",
                    "msg_parent",
                    "call_workflow",
                    "build",
                    Arc::new(AllowAll),
                    Arc::new(NeverInterrupted),
                )
                .with_orchestration_snapshot(Arc::clone(&snapshot)),
            )
            .await
            .expect("workflow runs");
        assert!(output.output.contains("<workflow name=\"release\""));
        let requests = host
            .requests
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].max_parallel, 2);
        assert_eq!(requests[0].nodes[1].depends_on, vec!["scan".to_owned()]);
        assert_eq!(requests[0].nodes[0].turn.agent, "explore");
        assert!(Arc::ptr_eq(
            requests[0]
                .parent_attempt
                .as_ref()
                .expect("workflow parent attempt"),
            &snapshot
        ));
        for node in &requests[0].nodes {
            assert!(Arc::ptr_eq(
                node.turn
                    .parent_attempt
                    .as_ref()
                    .expect("workflow node parent attempt"),
                &snapshot
            ));
            assert_eq!(node.turn.workflow.as_deref(), Some("release"));
            assert_eq!(node.turn.workflow_node.as_deref(), Some(node.id.as_str()));
        }
        assert!(
            requests[0].nodes[0]
                .turn
                .prompt
                .contains("Collect evidence.")
        );
        assert!(!requests[0].nodes[0].turn.background);
    }

    #[test]
    fn rejects_templates_outside_the_current_delegate_allowlist() {
        let task = TaskTool::new(
            Arc::new(crate::task::RecordingHost::new()),
            Arc::new(crate::task::NoProviders),
        )
        .with_targets(crate::task::DelegationTargets::new(["worker".to_owned()]).expect("targets"));
        let error = WorkflowTool::new(
            [template()],
            task,
            Arc::new(RecordingWorkflowHost::default()),
        )
        .err()
        .expect("explore is not delegated");
        assert!(error.contains("delegate allowlist"));
    }
}
