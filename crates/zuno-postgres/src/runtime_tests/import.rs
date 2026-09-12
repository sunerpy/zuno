use super::*;
use zuno_application::{
    environment::{Environment, EnvironmentSnapshot, EnvironmentSpec},
    workspace_import::*,
};
use zuno_types::{
    activity::Counter,
    identity::{EnvironmentId, GatewayId},
};
fn spec(session: &SessionId) -> EnvironmentSpec {
    EnvironmentSpec {
        id: EnvironmentId::new(session.as_str()).unwrap(),
        session_id: session.clone(),
        image: format!("fixture@sha256:{}", "a".repeat(64)),
        memory_bytes: 67108864,
        pids_limit: 32,
        cpu_millis: 500,
    }
}
fn request(id: &str) -> BeginWorkspaceImport {
    BeginWorkspaceImport {
        request_id: RequestId::new(id).unwrap(),
        expected_input_version: Counter(0),
        sha256: "a".repeat(64),
        bytes: Counter(1024),
    }
}
pub(super) async fn exercise(backend: &PostgresBackend, admin: &PgPool) {
    let actor = principal("workspace-import", "owner");
    crate::tests::install_access(admin, &actor).await;
    let session = session(backend, &actor, "initial").await;
    let configuration = submission(&session, "later", 0).configuration;
    let gateway = GatewayId::new("gateway").unwrap();
    let prepared = backend
        .begin_import(
            &actor,
            &session,
            request("archive"),
            configuration.clone(),
            gateway.clone(),
            spec(&session),
        )
        .await
        .unwrap();
    assert_eq!(prepared.state, WorkspaceImportState::Uploading);
    assert_eq!(
        backend
            .begin_import(
                &actor,
                &session,
                request("archive"),
                configuration.clone(),
                gateway.clone(),
                spec(&session)
            )
            .await
            .unwrap()
            .id,
        prepared.id
    );
    let mut changed = request("archive");
    changed.sha256 = "b".repeat(64);
    assert!(matches!(
        backend
            .begin_import(
                &actor,
                &session,
                changed,
                configuration.clone(),
                gateway.clone(),
                spec(&session)
            )
            .await,
        Err(ApplicationError::Conflict)
    ));
    let runtime = backend.runtime(actor.tenant_id().clone());
    assert!(matches!(
        runtime
            .submit(&actor, submission(&session, "blocked", 0))
            .await,
        Err(ApplicationError::Conflict)
    ));
    assert_eq!(
        runtime
            .input_version(&actor.owner(), &session)
            .await
            .unwrap(),
        0,
        "rejected early input cannot be consumed"
    );
    let assigned = backend
        .import_assignment(&actor, &session, &prepared.id)
        .await
        .unwrap();
    assert!(
        backend
            .authorize_workspace_initialization(&GatewayId::new("other").unwrap(), &assigned)
            .await
            .is_err()
    );
    backend
        .authorize_workspace_initialization(&gateway, &assigned)
        .await
        .unwrap();
    assert!(matches!(
        backend.cancel_import(&actor, &session, &prepared.id).await,
        Err(ApplicationError::Conflict)
    ));
    let receipt = WorkspaceImportReceipt {
        import_id: prepared.id.clone(),
        archive_sha256: assigned.sha256.clone(),
        snapshot: EnvironmentSnapshot {
            id: assigned.snapshot_id(),
            environment_id: assigned.source_environment_id(),
            revision: 1,
            sha256: "c".repeat(64),
            bytes: 1024,
        },
        environment: Environment {
            owner: actor.owner(),
            spec: assigned.environment.clone(),
            revision: 1,
        },
    };
    backend
        .complete_workspace_import(&gateway, &actor.owner(), &session, &receipt)
        .await
        .unwrap();
    backend
        .complete_workspace_import(&gateway, &actor.owner(), &session, &receipt)
        .await
        .unwrap();
    let mut wrong = receipt.clone();
    wrong.snapshot.sha256 = "d".repeat(64);
    assert!(matches!(
        backend
            .complete_workspace_import(&gateway, &actor.owner(), &session, &wrong)
            .await,
        Err(ApplicationError::Conflict)
    ));
    assert_eq!(
        backend
            .import_view(&actor, &session, &prepared.id)
            .await
            .unwrap()
            .state,
        WorkspaceImportState::Ready
    );
    assert_eq!(
        backend
            .imported_workspace(&actor.owner(), &session)
            .await
            .unwrap()
            .unwrap()
            .1,
        receipt
    );
    runtime
        .submit(&actor, submission(&session, "first-real-input", 0))
        .await
        .unwrap();
    assert!(
        backend
            .begin_import(
                &actor,
                &session,
                request("replace"),
                configuration,
                gateway,
                spec(&session)
            )
            .await
            .is_err()
    );

    let actor = principal("workspace-import-cancel", "owner");
    crate::tests::install_access(admin, &actor).await;
    let session = super::session(backend, &actor, "initial").await;
    let configuration = submission(&session, "later", 0).configuration;
    let gateway = GatewayId::new("gateway").unwrap();
    let prepared = backend
        .begin_import(
            &actor,
            &session,
            request("first"),
            configuration.clone(),
            gateway.clone(),
            spec(&session),
        )
        .await
        .unwrap();
    assert_eq!(
        backend
            .cancel_import(&actor, &session, &prepared.id)
            .await
            .unwrap()
            .state,
        WorkspaceImportState::Cancelled
    );
    assert!(
        backend
            .import_assignment(&actor, &session, &prepared.id)
            .await
            .is_err()
    );
    backend
        .begin_import(
            &actor,
            &session,
            request("replacement"),
            configuration,
            gateway,
            spec(&session),
        )
        .await
        .unwrap();

    let actor = principal("workspace-import-race", "owner");
    crate::tests::install_access(admin, &actor).await;
    let session = super::session(backend, &actor, "initial").await;
    let runtime = backend.runtime(actor.tenant_id().clone());
    let input = submission(&session, "racing-input", 0);
    let (import, input) = tokio::join!(
        backend.begin_import(
            &actor,
            &session,
            request("racing-archive"),
            input.configuration.clone(),
            GatewayId::new("gateway").unwrap(),
            spec(&session)
        ),
        runtime.submit(&actor, input.clone())
    );
    assert!(
        matches!(
            (&import, &input),
            (Ok(_), Err(ApplicationError::Conflict)) | (Err(ApplicationError::Conflict), Ok(_))
        ),
        "import and first input must have one winner: {import:?} {input:?}"
    );
}
