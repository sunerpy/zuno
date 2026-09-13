use super::*;
use zuno_application::workspace_files::{
    WorkspaceFileOperation, WorkspaceFileQuery, WorkspaceFileResult,
};

pub(super) async fn human_read(
    backend: &PostgresBackend,
    actor: &PrincipalScope,
    configuration: &ConfigurationRef,
    worker: &WorkerClient,
    client: &GatewayClient,
) {
    let principal = PrincipalScope::new(
        actor.tenant_id().clone(),
        actor.principal_id().clone(),
        PrincipalKind::User,
        Some(ClientId::new("unlisted-read").unwrap()),
        actor.policy_revision(),
    );
    let session = AgentApplication::new(Arc::new(backend.sessions(principal.clone())))
        .create_session(CreateSession {
            request_id: RequestId::new("human-file-session").unwrap(),
            workspace_id: WorkspaceId::new("workspace").unwrap(),
            title: "File HITL".to_owned(),
        })
        .await
        .unwrap();
    let job = backend
        .runtime(actor.tenant_id().clone())
        .submit(
            &principal,
            JobSubmission {
                selection: None,
                session_id: session.id,
                request_id: RequestId::new("human-file-job").unwrap(),
                expected_input_version: 0,
                text: "List files".to_owned(),
                configuration: configuration.clone(),
            },
        )
        .await
        .unwrap();
    let execution = worker
        .claim(
            WorkerInstanceId::new("human-file-worker").unwrap(),
            std::slice::from_ref(configuration),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(execution.job.id, job.id);
    let GatewayReply::Environment(environment) =
        reply(worker, &execution, client, GatewayCommand::Acquire)
            .await
            .unwrap()
    else {
        panic!("environment");
    };
    let operation = WorkspaceFileOperation {
        id: OperationId::new("human-file-list").unwrap(),
        invocation_id: InvocationId::new("human-file-list").unwrap(),
        environment_id: environment.spec.id,
        expected_revision: environment.revision,
        query: WorkspaceFileQuery::List {
            path: zuno_application::workspace_merge::WorkspacePath::root(),
            after: None,
            limit: 10,
        },
    };
    let GatewayReply::Approval(approval) = reply(
        worker,
        &execution,
        client,
        GatewayCommand::PrepareFiles {
            operation: operation.clone(),
        },
    )
    .await
    .unwrap() else {
        panic!("approval");
    };
    assert_eq!(approval.state, ApprovalState::Pending);
    assert!(
        reply(
            worker,
            &execution,
            client,
            GatewayCommand::QueryFiles {
                operation: operation.clone()
            }
        )
        .await
        .is_err()
    );
    backend
        .organizations(actor.tenant_id().clone())
        .answer(
            actor,
            AnswerApproval {
                request_id: RequestId::new("approve-human-file").unwrap(),
                approval_id: approval.id,
                answer: ApprovalAnswer::Approve,
            },
        )
        .await
        .unwrap();
    let GatewayReply::Files(result) = reply(
        worker,
        &execution,
        client,
        GatewayCommand::QueryFiles {
            operation: operation.clone(),
        },
    )
    .await
    .unwrap() else {
        panic!("files");
    };
    result.validate_for(&operation).unwrap();
    assert!(matches!(result.result,WorkspaceFileResult::List{entries,..} if entries.is_empty()));
    use zuno_application::control::{CancelJob, RuntimeControl};
    backend
        .runtime(actor.tenant_id().clone())
        .cancel(
            &principal,
            &job.id,
            CancelJob {
                request_id: RequestId::new("cancel-human-file").unwrap(),
                expected_turn_id: job.turn_id,
                reason: "finished".to_owned(),
            },
        )
        .await
        .unwrap();
    assert!(
        reply(
            worker,
            &execution,
            client,
            GatewayCommand::QueryFiles { operation }
        )
        .await
        .is_err(),
        "the old approval does not survive cancellation of execution authority"
    );
}
