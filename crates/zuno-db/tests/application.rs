use std::num::{NonZeroU64, NonZeroUsize};
use std::sync::Arc;

use zuno_application::{
    AgentApplication, ApplicationError, CreateSession, InputState, PageSize, QueueText,
    SessionPageRequest,
};
use zuno_db::application::{LocalWorkspace, SqliteSessionPersistence};
use zuno_db::event_log::SessionEventLog;
use zuno_db::inbox::{DurableInputKind, SessionInbox};
use zuno_db::{Pool, migration};
use zuno_paths::DbLocation;
use zuno_types::identity::{
    ClientId, PrincipalId, PrincipalKind, PrincipalScope, ProjectId, RequestId, WorkspaceId,
};

fn principal(tenant: &str, subject: &str) -> PrincipalScope {
    PrincipalScope::new(
        zuno_types::identity::TenantId::new(tenant).unwrap(),
        PrincipalId::new(subject).unwrap(),
        PrincipalKind::User,
        Some(ClientId::new("enterprise-web").unwrap()),
        NonZeroU64::MIN,
    )
}

struct Fixture {
    _directory: tempfile::TempDir,
    pool: Arc<Pool>,
    alice: AgentApplication,
    bob: AgentApplication,
    other_alice: AgentApplication,
}

fn fixture() -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let pool =
        Arc::new(Pool::open(&DbLocation::File(directory.path().join("preview.db"))).unwrap());
    {
        let mut connection = pool.get().unwrap();
        migration::apply(&mut connection).unwrap();
        connection.execute(
            "INSERT INTO project(id,worktree,time_created,time_updated,sandboxes) VALUES('project',?1,1,1,'[]')",
            [directory.path().to_str().unwrap()],
        ).unwrap();
    }
    let alice = principal("org-a", "alice");
    let bob = principal("org-a", "bob");
    let other_alice = principal("org-b", "alice");
    let workspaces = [
        ("workspace-a", alice.owner()),
        ("workspace-b", bob.owner()),
        ("workspace-c", other_alice.owner()),
    ]
    .into_iter()
    .map(|(id, owner)| LocalWorkspace {
        id: WorkspaceId::new(id).unwrap(),
        owner,
        project_id: ProjectId::new("project").unwrap(),
        root: directory.path().to_owned(),
    })
    .collect();
    let provider = SqliteSessionPersistence::new(
        pool.clone(),
        alice,
        workspaces,
        NonZeroUsize::new(4).unwrap(),
    )
    .unwrap();
    Fixture {
        _directory: directory,
        pool,
        bob: AgentApplication::new(Arc::new(provider.for_principal(bob))),
        other_alice: AgentApplication::new(Arc::new(provider.for_principal(other_alice))),
        alice: AgentApplication::new(Arc::new(provider)),
    }
}

fn create(request_id: &str, workspace: &str, title: &str) -> CreateSession {
    CreateSession {
        request_id: RequestId::new(request_id).unwrap(),
        workspace_id: WorkspaceId::new(workspace).unwrap(),
        title: title.to_owned(),
    }
}

#[tokio::test]
async fn application_views_isolate_sessions_inputs_and_workspace_selection() {
    let fixture = fixture();
    let alice = fixture
        .alice
        .create_session(create("same-request", "workspace-a", "Alice"))
        .await
        .unwrap();
    let other_alice = fixture
        .other_alice
        .create_session(create(
            "same-request",
            "workspace-c",
            "Another organization",
        ))
        .await
        .unwrap();
    assert_ne!(alice.id, other_alice.id);
    assert!(matches!(
        fixture.bob.session(&alice.id).await,
        Err(ApplicationError::NotFound)
    ));
    assert!(matches!(
        fixture.other_alice.session(&alice.id).await,
        Err(ApplicationError::NotFound)
    ));
    assert!(matches!(
        fixture
            .bob
            .create_session(create("forged", "workspace-a", "Not owned"))
            .await,
        Err(ApplicationError::NotFound)
    ));
    assert!(matches!(
        fixture
            .bob
            .queue_text(QueueText {
                request_id: RequestId::new("forged-input").unwrap(),
                session_id: alice.id.clone(),
                text: "Run a command".to_owned(),
            })
            .await,
        Err(ApplicationError::NotFound)
    ));
    assert!(
        SessionInbox::new(fixture.pool.clone())
            .pending(alice.id.as_str())
            .unwrap()
            .is_empty()
    );
    let page = fixture
        .alice
        .sessions(SessionPageRequest::default())
        .await
        .unwrap();
    assert_eq!(page.items.as_slice(), std::slice::from_ref(&alice));
    let encoded = serde_json::to_value(alice).unwrap();
    for internal in ["directory", "root", "permission", "principal", "metadata"] {
        assert!(
            encoded.get(internal).is_none(),
            "client DTO must not expose {internal}"
        );
    }
}

#[tokio::test]
async fn concurrent_creation_is_idempotent_and_a_changed_request_cannot_reassign_it() {
    let fixture = fixture();
    let request = create("one-request", "workspace-a", "Original title");
    let (first, second) = tokio::join!(
        fixture.alice.create_session(request.clone()),
        fixture.alice.create_session(request.clone()),
    );
    let first = first.unwrap();
    assert_eq!(first, second.unwrap());
    let log = SessionEventLog::new(fixture.pool.clone());
    let events = log.read_after(first.id.as_str(), None).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event_type, "application.session.created");
    assert_eq!(
        events[0].properties["principal"]["clientId"],
        "enterprise-web"
    );
    assert!(matches!(
        fixture
            .alice
            .create_session(create("one-request", "workspace-a", "Changed"))
            .await,
        Err(ApplicationError::Conflict)
    ));
    // An authorized title edit does not erase the original request receipt.
    fixture
        .pool
        .get()
        .unwrap()
        .execute(
            "UPDATE session SET title='Edited later' WHERE id=?1",
            [first.id.as_str()],
        )
        .unwrap();
    assert_eq!(
        fixture.alice.create_session(request).await.unwrap().title,
        "Edited later"
    );
    assert_eq!(log.read_after(first.id.as_str(), None).unwrap(), events);
}

#[tokio::test]
async fn keyset_pages_keep_equal_timestamps_without_crossing_owners() {
    let fixture = fixture();
    let mut expected = Vec::new();
    for index in 0..7 {
        expected.push(
            fixture
                .alice
                .create_session(create(
                    &format!("alice-{index}"),
                    "workspace-a",
                    "Same timestamp",
                ))
                .await
                .unwrap()
                .id,
        );
    }
    for index in 0..3 {
        fixture
            .bob
            .create_session(create(&format!("bob-{index}"), "workspace-b", "Private"))
            .await
            .unwrap();
    }
    fixture
        .pool
        .get()
        .unwrap()
        .execute("UPDATE session SET time_updated=100", [])
        .unwrap();
    let mut after = None;
    let mut observed = Vec::new();
    loop {
        let page = fixture
            .alice
            .sessions(SessionPageRequest {
                after,
                limit: PageSize::new(2).unwrap(),
            })
            .await
            .unwrap();
        observed.extend(page.items.into_iter().map(|session| session.id));
        after = page.next;
        if after.is_none() {
            break;
        }
        assert!(observed.len() <= 7, "cursor must advance");
    }
    expected.sort_by(|left, right| right.cmp(left));
    assert_eq!(observed, expected);
}

#[tokio::test]
async fn queued_input_keeps_native_shape_identity_and_one_durable_admission() {
    let fixture = fixture();
    let session = fixture
        .alice
        .create_session(create("create", "workspace-a", "Investigate"))
        .await
        .unwrap();
    fixture
        .pool
        .get()
        .unwrap()
        .execute(
            "UPDATE session SET agent='deep',model=?2 WHERE id=?1",
            (
                session.id.as_str(),
                zuno_db::session::model_reference("provider", "model"),
            ),
        )
        .unwrap();
    let input = QueueText {
        request_id: RequestId::new("input-one").unwrap(),
        session_id: session.id.clone(),
        text: "Investigate the runtime".to_owned(),
    };
    let (first, repeated) = tokio::join!(
        fixture.alice.queue_text(input.clone()),
        fixture.alice.queue_text(input.clone()),
    );
    let first = first.unwrap();
    assert_eq!(first, repeated.unwrap());
    assert_eq!(first.state, InputState::Queued);
    let inbox = SessionInbox::new(fixture.pool.clone());
    let pending = inbox.pending(session.id.as_str()).unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(
        DurableInputKind::classify(&pending[0].prompt),
        Some(DurableInputKind::User)
    );
    assert_eq!(pending[0].prompt["agent"], "deep");
    assert_eq!(pending[0].prompt["model"]["providerId"], "provider");
    assert_eq!(pending[0].prompt["model"]["modelId"], "model");
    assert_eq!(pending[0].prompt["prompt"]["text"], input.text);
    assert_eq!(
        pending[0].trigger_kind,
        zuno_types::execution::InputTriggerKind::User
    );
    assert!(matches!(
        fixture
            .alice
            .queue_text(QueueText {
                text: "Different request".to_owned(),
                ..input.clone()
            })
            .await,
        Err(ApplicationError::Conflict)
    ));
    let events = SessionEventLog::new(fixture.pool.clone())
        .read_after(session.id.as_str(), None)
        .unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "session.input.admitted")
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "application.input.queued")
            .count(),
        1
    );
    assert_eq!(
        events.last().unwrap().properties["principal"]["principalId"],
        "alice"
    );
    assert_eq!(
        first.admitted_cursor,
        pending[0].admitted_sequence.to_string()
    );
}

#[tokio::test]
async fn a_failed_creation_audit_rolls_back_the_session_and_ownership() {
    let fixture = fixture();
    fixture
        .pool
        .get()
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER fail_application_create BEFORE INSERT ON event
         WHEN NEW.type LIKE 'application.session.created%'
         BEGIN SELECT RAISE(ABORT,'injected audit failure'); END;",
        )
        .unwrap();
    assert!(
        fixture
            .alice
            .create_session(create("failed", "workspace-a", "Atomic"))
            .await
            .is_err()
    );
    let connection = fixture.pool.get().unwrap();
    for table in ["session", "session_ownership", "event", "event_sequence"] {
        let rows: i64 = connection
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(rows, 0, "{table} must roll back");
    }
}

#[tokio::test]
async fn a_failed_input_audit_rolls_back_its_admission_and_sequence() {
    let fixture = fixture();
    let session = fixture
        .alice
        .create_session(create("create", "workspace-a", "Atomic"))
        .await
        .unwrap();
    let log = SessionEventLog::new(fixture.pool.clone());
    let before = log.read_after(session.id.as_str(), None).unwrap();
    fixture
        .pool
        .get()
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER fail_application_input BEFORE INSERT ON event
         WHEN NEW.type LIKE 'application.input.queued%'
         BEGIN SELECT RAISE(ABORT,'injected audit failure'); END;",
        )
        .unwrap();
    let input = QueueText {
        request_id: RequestId::new("atomic-input").unwrap(),
        session_id: session.id.clone(),
        text: "Task".to_owned(),
    };
    assert!(fixture.alice.queue_text(input.clone()).await.is_err());
    assert_eq!(log.read_after(session.id.as_str(), None).unwrap(), before);
    assert!(
        SessionInbox::new(fixture.pool.clone())
            .pending(session.id.as_str())
            .unwrap()
            .is_empty()
    );
    fixture
        .pool
        .get()
        .unwrap()
        .execute_batch("DROP TRIGGER fail_application_input")
        .unwrap();
    let admitted = fixture.alice.queue_text(input).await.unwrap();
    assert_eq!(
        admitted.admitted_cursor,
        (before.last().unwrap().sequence + 1).to_string()
    );
}

#[tokio::test]
async fn the_application_is_a_native_component_and_validates_before_persistence() {
    let fixture = fixture();
    let runtime = zuno_runtime::HarnessRuntime::new("application");
    runtime.mount(fixture.alice.clone()).await.unwrap();
    let app = runtime.service::<AgentApplication>().unwrap();
    assert_eq!(app.principal(), fixture.alice.principal());
    assert!(matches!(
        app.create_session(create("blank", "workspace-a", "   "))
            .await,
        Err(ApplicationError::Invalid(_))
    ));
    assert!(matches!(
        app.create_session(create("control", "workspace-a", "line\nbreak"))
            .await,
        Err(ApplicationError::Invalid(_))
    ));
    assert!(
        app.sessions(SessionPageRequest::default())
            .await
            .unwrap()
            .items
            .is_empty()
    );
}
