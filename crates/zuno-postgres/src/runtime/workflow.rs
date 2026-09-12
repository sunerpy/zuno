//! Durable DAG coordination, serialized only while examining dependencies and
//! admitting nodes. Node execution uses the existing independently leased Jobs.

mod admission;
mod coordinator;
mod inputs;
use coordinator::advance;
pub(super) use coordinator::{drain, result};
use inputs::resolve_input;
pub(super) use inputs::resolved_input;

use super::*;
use serde::{Deserialize, Serialize};
use zuno_application::{
    child::{
        ChildDefinitionGrant, ChildDelivery, ChildInvocation, ChildWorkspacePolicy,
        ChildWorkspaceState,
    },
    runtime::JobInputSelection,
    workflow::{
        WorkflowDefinitionGrant, WorkflowDispatch, WorkflowInvocation, WorkflowNodeDispatch,
        WorkflowStore,
    },
};
use zuno_engine::workflow::{Decision, NodePhase, WorkflowGraph};
use zuno_orchestration::WorkflowTemplateDescriptor;
use zuno_types::identity::{InvocationId, NodeRunId, WorkflowRunId};

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct NodeDefinition {
    id: String,
    configuration: ConfigurationRef,
    selection: JobInputSelection,
    maximum_depth: u32,
    maximum_children: u32,
    workspace: ChildWorkspacePolicy,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Plan {
    template: WorkflowTemplateDescriptor,
    invocation: WorkflowInvocation,
    nodes: Vec<NodeDefinition>,
}
impl Plan {
    fn new(
        invocation: WorkflowInvocation,
        grant: &WorkflowDefinitionGrant,
    ) -> Result<Self, ApplicationError> {
        invocation.validate()?;
        grant.group.validate()?;
        if grant.template.name != invocation.template
            || grant.group.parent != grant.group.child
            || grant.template.nodes.len() > grant.template.max_agents
            || grant.template.max_agents > 64
            || grant.template.max_parallel > grant.template.max_agents
            || grant.nodes.len() != grant.template.nodes.len()
        {
            return Err(invalid(
                "workflow grant does not match its immutable template",
            ));
        }
        let nodes = grant
            .template
            .nodes
            .iter()
            .map(|node| {
                let child = grant
                    .nodes
                    .get(&node.id)
                    .ok_or(ApplicationError::Forbidden)?;
                child.validate()?;
                if child.parent != grant.group.child || child.selection.agent != node.agent {
                    return Err(ApplicationError::Forbidden);
                }
                if child.workspace == ChildWorkspacePolicy::ForkParent
                    && grant.group.workspace != ChildWorkspacePolicy::ForkParent
                {
                    return Err(invalid(
                        "workflow node workspaces require a prepared group workspace",
                    ));
                }
                Ok(NodeDefinition {
                    id: node.id.clone(),
                    configuration: child.child.clone(),
                    selection: child.selection.clone(),
                    maximum_depth: child.maximum_depth,
                    maximum_children: child.maximum_children,
                    workspace: child.workspace,
                })
            })
            .collect::<Result<Vec<_>, ApplicationError>>()?;
        let plan = Self {
            template: grant.template.clone(),
            invocation,
            nodes,
        };
        plan.graph()?;
        if serde_json::to_vec(&plan)
            .map_err(ApplicationError::storage)?
            .len()
            > 2 * 1024 * 1024
        {
            return Err(invalid("workflow plan exceeds its bound"));
        }
        Ok(plan)
    }
    fn graph(&self) -> Result<WorkflowGraph, ApplicationError> {
        if self.nodes.len() != self.template.nodes.len()
            || self
                .nodes
                .iter()
                .zip(&self.template.nodes)
                .any(|(node, template)| {
                    node.id != template.id || node.selection.agent != template.agent
                })
        {
            return Err(invalid(
                "stored workflow nodes disagree with their definition",
            ));
        }
        WorkflowGraph::new(
            self.template
                .nodes
                .iter()
                .map(|node| (node.id.clone(), node.depends_on.clone())),
            self.template.max_parallel,
        )
        .map_err(ApplicationError::storage)
    }
}
struct Run {
    id: WorkflowRunId,
    job: JobId,
    parent: JobId,
    plan: Plan,
    state: String,
}

fn invalid(message: &str) -> ApplicationError {
    ApplicationError::Invalid(message.to_owned())
}

async fn read(
    tx: &mut Transaction<'_, Postgres>,
    owner: &PrincipalKey,
    job: &JobId,
) -> Result<Run, ApplicationError> {
    let row = query("SELECT * FROM zuno_enterprise_preview.runtime_workflow WHERE tenant_id=$1 AND principal_id=$2 AND job_id=$3 FOR UPDATE")
        .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(job.as_str())
        .fetch_one(&mut **tx).await.map_err(database_error)?;
    let raw: Value = row.try_get("plan").map_err(database_error)?;
    if row
        .try_get::<String, _>("plan_digest")
        .map_err(database_error)?
        != zuno_orchestration::sha256_json(&raw)
    {
        return Err(invalid("workflow definition digest disagrees"));
    }
    let plan: Plan = serde_json::from_value(raw).map_err(ApplicationError::storage)?;
    plan.graph()?;
    Ok(Run {
        id: WorkflowRunId::new(row.try_get::<String, _>("run_id").map_err(database_error)?)
            .map_err(ApplicationError::storage)?,
        job: job.clone(),
        parent: JobId::new(
            row.try_get::<String, _>("parent_job_id")
                .map_err(database_error)?,
        )
        .map_err(ApplicationError::storage)?,
        plan,
        state: row.try_get("state").map_err(database_error)?,
    })
}

async fn parent(
    tx: &mut Transaction<'_, Postgres>,
    lease: &ExecutionLease,
) -> Result<RuntimeJob, ApplicationError> {
    let job = verify_lease(tx, lease).await?;
    let access = crate::authorization::access_in(tx, &lease.owner).await?;
    if zuno_permission::enterprise::actor_denial(&access.policy, &access.member, &job.principal)
        .is_some()
    {
        return Err(ApplicationError::Forbidden);
    }
    Ok(job)
}

async fn dispatch_view(
    tx: &mut Transaction<'_, Postgres>,
    owner: &PrincipalKey,
    run: &Run,
) -> Result<WorkflowDispatch, ApplicationError> {
    let group = children::read(tx, owner, &run.job).await?.ticket;
    let rows = query(
        "SELECT node_id,child_job_id FROM zuno_enterprise_preview.runtime_workflow_node
        WHERE tenant_id=$1 AND principal_id=$2 AND run_id=$3 ORDER BY position",
    )
    .bind(owner.tenant_id.as_str())
    .bind(owner.principal_id.as_str())
    .bind(run.id.as_str())
    .fetch_all(&mut **tx)
    .await
    .map_err(database_error)?;
    let mut nodes = Vec::new();
    for row in rows {
        let id = JobId::new(
            row.try_get::<String, _>("child_job_id")
                .map_err(database_error)?,
        )
        .map_err(ApplicationError::storage)?;
        nodes.push(WorkflowNodeDispatch {
            node_id: row.try_get("node_id").map_err(database_error)?,
            child: children::read(tx, owner, &id).await?.ticket,
        });
    }
    Ok(WorkflowDispatch {
        run_id: run.id.clone(),
        group,
        nodes,
        prepared: run.state != "preparing",
    })
}

async fn set_state(
    tx: &mut Transaction<'_, Postgres>,
    coordinator: &RuntimeJob,
    run: &Run,
    state: &str,
) -> Result<(), ApplicationError> {
    let now = database_time(tx).await?;
    query("UPDATE zuno_enterprise_preview.runtime_workflow SET state=$4,revision=revision+1,time_updated=$5
        WHERE tenant_id=$1 AND principal_id=$2 AND run_id=$3")
        .bind(coordinator.principal.tenant_id().as_str()).bind(coordinator.principal.principal_id().as_str()).bind(run.id.as_str())
        .bind(state).bind(now).execute(&mut **tx).await.map_err(database_error)?;
    emit(
        tx,
        &coordinator.principal,
        coordinator.session_id.as_str(),
        "runtime.workflow.state",
        json!({"runId":run.id,"jobId":run.job,"state":state}),
    )
    .await?;
    Ok(())
}

async fn activate(
    tx: &mut Transaction<'_, Postgres>,
    coordinator: &RuntimeJob,
    run: &Run,
) -> Result<(), ApplicationError> {
    if run.state != "prepared" {
        return Err(ApplicationError::Conflict);
    }
    set_state(tx, coordinator, run, "active").await?;
    // Node admission and dependency advancement are performed in this same
    // transaction by the shared coordinator below.
    Box::pin(advance(tx, coordinator, run)).await
}

pub(super) async fn identity(
    tx: &mut Transaction<'_, Postgres>,
    owner: &PrincipalKey,
    job: &JobId,
) -> Result<Option<(WorkflowRunId, String)>, ApplicationError> {
    let row = query("SELECT run_id,plan->'template'->>'name' AS name FROM zuno_enterprise_preview.runtime_workflow
        WHERE tenant_id=$1 AND principal_id=$2 AND job_id=$3")
        .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(job.as_str())
        .fetch_optional(&mut **tx).await.map_err(database_error)?;
    row.map(|row| {
        Ok((
            WorkflowRunId::new(row.try_get::<String, _>("run_id").map_err(database_error)?)
                .map_err(ApplicationError::storage)?,
            row.try_get("name").map_err(database_error)?,
        ))
    })
    .transpose()
}

/// Runs inside the original Agent's wait/checkpoint transaction.
pub(super) async fn activate_wait(
    tx: &mut Transaction<'_, Postgres>,
    parent: &RuntimeJob,
    group: &JobId,
) -> Result<(), ApplicationError> {
    if identity(tx, &parent.principal.owner(), group)
        .await?
        .is_none()
    {
        return Ok(());
    }
    let run = read(tx, &parent.principal.owner(), group).await?;
    if run.parent != parent.id || run.plan.invocation.root.delivery != ChildDelivery::Foreground {
        return Err(ApplicationError::Forbidden);
    }
    let coordinator = read_job(tx, &parent.principal.owner(), group.as_str()).await?;
    super::control::lock_session(tx, &coordinator).await?;
    if run.state == "prepared" {
        activate(tx, &coordinator, &run).await?;
    } else if run.state != "active" {
        return Err(ApplicationError::Conflict);
    }
    Ok(())
}

/// The original parent may prepare only nodes of its own unstarted workflow.
/// This does not create a Worker lease for the logical coordinator.
pub(super) async fn preparation_parent(
    tx: &mut Transaction<'_, Postgres>,
    parent: &RuntimeJob,
    group: &JobId,
) -> Result<Option<RuntimeJob>, ApplicationError> {
    if identity(tx, &parent.principal.owner(), group)
        .await?
        .is_none()
    {
        return Ok(None);
    }
    let run = read(tx, &parent.principal.owner(), group).await?;
    if run.parent != parent.id || !matches!(run.state.as_str(), "preparing" | "prepared") {
        return Err(ApplicationError::Forbidden);
    }
    let coordinator = read_job(tx, &parent.principal.owner(), group.as_str()).await?;
    super::control::lock_session(tx, &coordinator).await?;
    Ok(Some(coordinator))
}
