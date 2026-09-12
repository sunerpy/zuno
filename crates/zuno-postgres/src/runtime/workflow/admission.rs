//! Original-parent lease authorization and idempotent graph preparation.
use super::*;

#[async_trait]
impl WorkflowStore for PostgresRuntimeStore {
    async fn dispatch_workflow(
        &self,
        lease: &ExecutionLease,
        invocation: WorkflowInvocation,
        grant: &WorkflowDefinitionGrant,
    ) -> Result<WorkflowDispatch, ApplicationError> {
        self.check_owner(&lease.owner)?;
        let plan = Plan::new(invocation, grant)?;
        let mut tx = owner_transaction(&self.pool, &lease.owner).await?;
        let parent = parent(&mut tx, lease).await?;
        let group = children::stage_in(
            &mut tx,
            &parent,
            plan.invocation.root.clone(),
            &grant.group,
            false,
        )
        .await?;
        let run_id = WorkflowRunId::new(format!(
            "wfr_{}",
            zuno_orchestration::sha256_json(&json!([lease.owner, group.job_id]))
        ))
        .map_err(ApplicationError::storage)?;
        let encoded = json!(plan);
        let digest = zuno_orchestration::sha256_json(&encoded);
        let now = database_time(&mut tx).await?;
        query("INSERT INTO zuno_enterprise_preview.runtime_workflow
            (tenant_id,principal_id,run_id,job_id,parent_job_id,parent_session_id,plan,plan_digest,state,time_created,time_updated)
            VALUES($1,$2,$3,$4,$5,$6,$7,$8,'preparing',$9,$9) ON CONFLICT DO NOTHING")
            .bind(lease.owner.tenant_id.as_str()).bind(lease.owner.principal_id.as_str()).bind(run_id.as_str())
            .bind(group.job_id.as_str()).bind(parent.id.as_str()).bind(parent.session_id.as_str())
            .bind(encoded).bind(&digest).bind(now).execute(&mut *tx).await.map_err(database_error)?;
        let run = read(&mut tx, &lease.owner, &group.job_id).await?;
        if zuno_orchestration::sha256_json(&json!(run.plan)) != digest
            || run.parent != parent.id
            || run.state == "cancelled"
        {
            return Err(ApplicationError::Conflict);
        }
        let view = dispatch_view(&mut tx, &lease.owner, &run).await?;
        verify_lease(&mut tx, lease).await?;
        tx.commit().await.map_err(database_error)?;
        Ok(view)
    }

    async fn prepare_workflow(
        &self,
        lease: &ExecutionLease,
        job: &JobId,
    ) -> Result<WorkflowDispatch, ApplicationError> {
        self.check_owner(&lease.owner)?;
        let mut tx = owner_transaction(&self.pool, &lease.owner).await?;
        let parent = parent(&mut tx, lease).await?;
        let mut run = read(&mut tx, &lease.owner, job).await?;
        if run.parent != parent.id
            || matches!(run.state.as_str(), "cancelled" | "failed" | "uncertain")
        {
            return Err(ApplicationError::Forbidden);
        }
        let group = children::read(&mut tx, &lease.owner, job).await?;
        if group.ticket.workspace == ChildWorkspaceState::Pending {
            return Err(ApplicationError::Conflict);
        }
        if group.state == "staged" {
            children::activate(&mut tx, &parent, &group).await?;
        }
        let coordinator = read_job(&mut tx, &lease.owner, job.as_str()).await?;
        super::control::lock_session(&mut tx, &coordinator).await?;
        if run.state == "preparing" {
            for (position, (definition, template)) in run
                .plan
                .nodes
                .iter()
                .zip(&run.plan.template.nodes)
                .enumerate()
            {
                let digest = zuno_orchestration::sha256_json(&json!([run.id, template.id]));
                let invocation = ChildInvocation {
                    invocation_id: InvocationId::new(format!("node_{digest}"))
                        .map_err(ApplicationError::storage)?,
                    arguments_sha256: zuno_orchestration::sha256_json(&json!([
                        run.plan.invocation,
                        template
                    ])),
                    logical_key: format!("workflow-node:{digest}"),
                    prompt: template.prompt.as_ref().map_or_else(
                        || run.plan.invocation.root.prompt.clone(),
                        |instruction| {
                            format!(
                                "{}\n\nWorkflow node `{}` instruction:\n{}",
                                run.plan.invocation.root.prompt, template.id, instruction
                            )
                        },
                    ),
                    description: template
                        .description
                        .clone()
                        .unwrap_or_else(|| format!("{} / {}", run.plan.template.name, template.id)),
                    delivery: ChildDelivery::Quiet,
                    resume_session_id: None,
                    presentation: json!({"workflow":run.plan.template.name,"runId":run.id,"node":template.id}),
                };
                let grant = ChildDefinitionGrant {
                    parent: coordinator.configuration.clone(),
                    child: definition.configuration.clone(),
                    selection: definition.selection.clone(),
                    maximum_depth: definition.maximum_depth,
                    maximum_children: definition.maximum_children,
                    workspace: definition.workspace,
                };
                let child =
                    children::stage_in(&mut tx, &coordinator, invocation, &grant, false).await?;
                let node_run_id =
                    NodeRunId::new(format!("node_{digest}")).map_err(ApplicationError::storage)?;
                let now = database_time(&mut tx).await?;
                query("INSERT INTO zuno_enterprise_preview.runtime_workflow_node
                    (tenant_id,principal_id,run_id,node_run_id,node_id,position,child_job_id,state,time_updated)
                    VALUES($1,$2,$3,$4,$5,$6,$7,'pending',$8) ON CONFLICT DO NOTHING")
                    .bind(lease.owner.tenant_id.as_str()).bind(lease.owner.principal_id.as_str()).bind(run.id.as_str()).bind(node_run_id.as_str())
                    .bind(&template.id).bind(i32::try_from(position).map_err(ApplicationError::storage)?).bind(child.job_id.as_str()).bind(now)
                    .execute(&mut *tx).await.map_err(database_error)?;
            }
            let view = dispatch_view(&mut tx, &lease.owner, &run).await?;
            if view
                .nodes
                .iter()
                .all(|node| node.child.workspace != ChildWorkspaceState::Pending)
            {
                set_state(&mut tx, &coordinator, &run, "prepared").await?;
                run.state = "prepared".to_owned();
            }
        }
        if run.state == "prepared" && run.plan.invocation.root.delivery != ChildDelivery::Foreground
        {
            activate(&mut tx, &coordinator, &run).await?;
            run.state = "active".to_owned();
        }
        let view = dispatch_view(&mut tx, &lease.owner, &run).await?;
        verify_lease(&mut tx, lease).await?;
        tx.commit().await.map_err(database_error)?;
        Ok(view)
    }
}
