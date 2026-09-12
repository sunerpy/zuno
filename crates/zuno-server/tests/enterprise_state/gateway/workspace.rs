use super::*;
use zuno_application::{
    child::{
        ChildDefinitionGrant, ChildDelivery, ChildDispatchStore, ChildInvocation,
        ChildWorkspacePolicy, ChildWorkspaceState,
    },
    runtime::{JobFinish, JobInputModel, JobInputSelection},
};
use zuno_worker::WorkerExecution;

pub(super) async fn exercise(
    context: &root::Context<'_>,
    parent: &WorkerExecution,
    gateway: &GatewayClient,
) -> (EnvironmentId, u64) {
    let runtime = context.backend.runtime(context.actor.tenant_id().clone());
    let lease = parent.lease().unwrap();
    let intent = ChildInvocation {
        invocation_id: InvocationId::new("workspace-child").unwrap(),
        arguments_sha256: "a".repeat(64),
        logical_key: "workspace child".to_owned(),
        prompt: "Inspect inherited files".to_owned(),
        description: "Workspace child".to_owned(),
        delivery: ChildDelivery::Quiet,
        resume_session_id: None,
        presentation: json!({"objective":"Inspect inherited files"}),
    };
    let grant = ChildDefinitionGrant {
        parent: context.configuration.clone(),
        child: context.configuration.clone(),
        selection: JobInputSelection {
            agent: "build".to_owned(),
            model: JobInputModel {
                provider_id: "wire-test".to_owned(),
                model_id: "model".to_owned(),
            },
        },
        maximum_depth: 2,
        maximum_children: 4,
        workspace: ChildWorkspacePolicy::ForkParent,
    };
    let ticket = runtime
        .dispatch_child(&lease, intent.clone(), &grant)
        .await
        .unwrap();
    assert_eq!(ticket.workspace, ChildWorkspaceState::Pending);
    assert!(
        runtime
            .get(&context.actor.owner(), &ticket.job_id)
            .await
            .is_err()
    );
    let request = GatewayRequest::new(GatewayCommand::PrepareChildWorkspace {
        child_job_id: ticket.job_id.clone(),
    })
    .unwrap();
    let issued = context
        .worker
        .gateway_ticket(parent, &request)
        .await
        .unwrap();
    assert!(
        gateway.execute(&issued, &request).await.is_err(),
        "the fixture loses the first post-commit workspace acknowledgement"
    );
    let saved = runtime
        .child_workspace(&lease, &ticket.job_id)
        .await
        .unwrap()
        .receipt
        .unwrap();
    let GatewayReply::ChildWorkspace(replayed) = gateway.execute(&issued, &request).await.unwrap()
    else {
        panic!("workspace");
    };
    assert_eq!(
        saved, replayed,
        "retry retrieves the exact prepared workspace without another copy"
    );
    let missing = GatewayRequest::new(GatewayCommand::PrepareChildWorkspace {
        child_job_id: JobId::new("not-this-parents-child").unwrap(),
    })
    .unwrap();
    assert!(
        context
            .worker
            .gateway_ticket(parent, &missing)
            .await
            .is_err()
    );
    let ready = runtime
        .dispatch_child(&parent.lease().unwrap(), intent, &grant)
        .await
        .unwrap();
    assert_eq!(ready.workspace, ChildWorkspaceState::Ready);
    let child = context
        .worker
        .claim(
            WorkerInstanceId::new("workspace-child-worker").unwrap(),
            std::slice::from_ref(context.configuration),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(child.job.id, ticket.job_id);
    let GatewayReply::Environment(environment) =
        reply(context.worker, &child, gateway, GatewayCommand::Acquire)
            .await
            .unwrap()
    else {
        panic!("child environment");
    };
    assert_eq!(environment.spec.id, saved.target.spec.id);
    let operation = CommandOperation {
        id: OperationId::new("child-workspace-proof").unwrap(),
        invocation_id: InvocationId::new("child-workspace-proof").unwrap(),
        environment_id: environment.spec.id.clone(),
        expected_revision: environment.revision,
        argv: vec![
            "sh".to_owned(),
            "-c".to_owned(),
            "test \"$(cat /workspace/once)\" = ran; printf child > /workspace/once".to_owned(),
        ],
    };
    let GatewayReply::Approval(approval) = reply(
        context.worker,
        &child,
        gateway,
        GatewayCommand::PrepareCommand {
            operation: operation.clone(),
        },
    )
    .await
    .unwrap() else {
        panic!("approval");
    };
    assert!(
        reply(
            context.worker,
            &child,
            gateway,
            GatewayCommand::SubmitCommand {
                operation: operation.clone()
            }
        )
        .await
        .is_err(),
        "workspace inheritance is not command approval"
    );
    context
        .backend
        .organizations(context.actor.tenant_id().clone())
        .answer(
            context.actor,
            AnswerApproval {
                request_id: RequestId::new("approve-child-workspace").unwrap(),
                approval_id: approval.id,
                answer: ApprovalAnswer::Approve,
            },
        )
        .await
        .unwrap();
    reply(
        context.worker,
        &child,
        gateway,
        GatewayCommand::SubmitCommand {
            operation: operation.clone(),
        },
    )
    .await
    .unwrap();
    let until = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        let GatewayReply::Operation(receipt) = reply(
            context.worker,
            &child,
            gateway,
            GatewayCommand::Inspect {
                operation_id: operation.id.clone(),
            },
        )
        .await
        .unwrap() else {
            panic!("operation");
        };
        if receipt.phase == OperationPhase::Completed {
            assert_eq!(receipt.exit_code, Some(0));
            break;
        }
        assert!(tokio::time::Instant::now() < until);
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let source = saved.snapshot.unwrap();
    let parent_after = context
        .delivery
        .environments()
        .snapshot(
            &context.actor.owner(),
            &source.environment_id,
            source.revision,
        )
        .await
        .unwrap();
    assert_eq!(
        source.sha256, parent_after.sha256,
        "a child's writes must not change the parent workspace"
    );
    runtime
        .finish(
            &child.lease().unwrap(),
            JobFinish::Cancelled {
                reason: "fixture cleanup".to_owned(),
            },
        )
        .await
        .unwrap();
    (environment.spec.id, environment.revision + 1)
}
