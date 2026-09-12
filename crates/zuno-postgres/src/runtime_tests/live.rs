use super::*;
use zuno_application::{
    control::{CancelJob, RuntimeControl},
    live::LiveUpdate,
};
use zuno_db::{assistant_commit::AssistantCommit, message::MessageRecord};
use zuno_engine::state::{TurnPersistence, TurnStateScope};
use zuno_types::activity::{LiveEvent, LiveItem};

pub(super) async fn exercise(backend: &PostgresBackend, admin: &PgPool) {
    let actor = principal("live-progress", "alice");
    crate::tests::install_access(admin, &actor).await;
    let session_id = session(backend, &actor, "session").await;
    let runtime = backend.runtime(actor.tenant_id().clone());
    let job = runtime
        .submit(&actor, submission(&session_id, "turn", 0))
        .await
        .unwrap();
    let claim = runtime
        .claim(&worker("first"), duration())
        .await
        .unwrap()
        .unwrap();
    let state = backend.worker_state(claim.lease.clone());
    let scope = TurnStateScope {
        owner: actor.owner(),
        session_id: session_id.to_string(),
    };
    let message = MessageRecord::from_json(json!({
        "id":"live-message","sessionID":session_id,"role":"assistant","time":{"created":100}
    }))
    .unwrap();
    state
        .commit_assistant(
            &scope,
            &AssistantCommit {
                message: message.clone(),
                parts: vec![],
                persisted_at_ms: 100,
                context_limit: None,
                context_usage: None,
            },
        )
        .await
        .unwrap();
    let update = LiveUpdate {
        generation: "generation-a".to_owned(),
        sequence: 1,
        message_id: Some(message.id.clone()),
        items: vec![LiveItem::Text {
            id: "draft".to_owned(),
            parent_id: Some("message:live-message".to_owned()),
            text: "Visible draft".to_owned(),
            truncated: false,
        }],
    };
    backend.publish_live(&claim.lease, &update).await.unwrap();
    backend.publish_live(&claim.lease, &update).await.unwrap();
    let frame = backend
        .live_progress(&actor, &session_id)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(frame.event,LiveEvent::Snapshot {items} if items.len()==1));
    let mut changed = update.clone();
    changed.items.clear();
    assert!(matches!(
        backend.publish_live(&claim.lease, &changed).await,
        Err(ApplicationError::Conflict)
    ));
    changed.sequence = 2;
    backend.publish_live(&claim.lease, &changed).await.unwrap();
    assert!(matches!(
        backend.publish_live(&claim.lease, &update).await,
        Err(ApplicationError::Conflict)
    ));
    let mut forged = update.clone();
    forged.sequence = 3;
    forged.message_id = Some("another-message".to_owned());
    forged.items.clear();
    assert!(matches!(
        backend.publish_live(&claim.lease, &forged).await,
        Err(ApplicationError::Conflict)
    ));
    let mut finished = message;
    finished
        .data
        .insert("time".to_owned(), json!({"created":100,"completed":200}));
    state
        .commit_assistant(
            &scope,
            &AssistantCommit {
                message: finished,
                parts: vec![],
                persisted_at_ms: 200,
                context_limit: None,
                context_usage: None,
            },
        )
        .await
        .unwrap();
    assert!(
        backend
            .live_progress(&actor, &session_id)
            .await
            .unwrap()
            .is_none(),
        "committed model output hides its old draft immediately"
    );
    runtime
        .cancel(
            &actor,
            &job.id,
            CancelJob {
                request_id: RequestId::new("cancel").unwrap(),
                expected_turn_id: job.turn_id,
                reason: "Finished observing".to_owned(),
            },
        )
        .await
        .unwrap();
    changed.message_id = None;
    changed.sequence = 3;
    assert!(matches!(
        backend.publish_live(&claim.lease, &changed).await,
        Err(ApplicationError::LeaseLost)
    ));
    let other = principal("live-progress", "bob");
    crate::tests::install_access(admin, &other).await;
    assert!(matches!(
        backend.live_progress(&other, &session_id).await,
        Err(ApplicationError::NotFound)
    ));
}
