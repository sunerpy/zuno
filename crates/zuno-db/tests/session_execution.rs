use std::sync::Arc;

use zuno_db::session_execution::SessionExecutionStore;
use zuno_db::{Pool, migration, session};
use zuno_paths::DbLocation;
use zuno_types::execution::{
    CollaborationMode, ContinuationToken, DraftReviewRiskAcceptance, SessionExecutionPhase,
    TurnExecutionIdentity,
};

const SESSION: &str = "ses_execution";

fn initialized() -> Arc<Pool> {
    let pool = Arc::new(Pool::open(&DbLocation::Memory).expect("pool"));
    {
        let mut connection = pool.get().expect("connection");
        migration::apply(&mut connection).expect("schema");
        connection
            .execute(
                "INSERT INTO project \
                 (id, worktree, time_created, time_updated, sandboxes) \
                 VALUES ('project', '/workspace', 1, 1, '[]')",
                [],
            )
            .expect("project");
    }
    pool.transaction(|transaction| {
        session::create(
            transaction,
            &session::SessionCreate::new(
                SESSION,
                "execution",
                "project",
                "/workspace",
                "/workspace",
                "Execution test",
                "zuno",
            )
            .at(1),
        )
        .map(|_| ())
    })
    .expect("session");
    pool
}

#[test]
fn execution_state_seeds_once_and_round_trips_continuation_identity() {
    let store = SessionExecutionStore::new(initialized());
    let identity =
        TurnExecutionIdentity::new("build", "provider", "model").with_reasoning(Some("high"));
    let seeded = store
        .seed(SESSION, CollaborationMode::Plan, Some(identity.clone()), 10)
        .expect("seed");
    assert_eq!(seeded.revision, 1);
    assert_eq!(seeded.phase, SessionExecutionPhase::Planning);
    assert_eq!(
        store
            .seed(SESSION, CollaborationMode::Work, None, 20)
            .expect("idempotent seed"),
        seeded
    );

    let mut next = seeded.clone();
    next.mode = CollaborationMode::Work;
    next.phase = SessionExecutionPhase::Authorized;
    next.authorized_plan_id = Some("plan_1".to_owned());
    next.authorized_plan_revision = Some(4);
    next.handoff_plan_id = Some("plan_1".to_owned());
    next.handoff_plan_revision = Some(4);
    next.draft_review_risk = Some(DraftReviewRiskAcceptance {
        review_id: "rev_1".to_owned(),
        review_revision: 2,
        reason: "accept the documented residual risk".to_owned(),
        time_accepted: 25,
    });
    next.cycle_id = Some("cycle_1".to_owned());
    next.continuation = Some(ContinuationToken {
        cycle_id: "cycle_1".to_owned(),
        identity,
        mode: CollaborationMode::Work,
        plan_id: Some("plan_1".to_owned()),
        plan_revision: Some(4),
        context_epoch: 2,
        anchor_message_id: Some("msg_anchor".to_owned()),
    });
    next.time_updated = 30;
    let updated = store.update(1, next).expect("update");

    assert_eq!(updated.revision, 2);
    assert_eq!(store.get(SESSION).expect("read"), Some(updated));
}

#[test]
fn execution_state_rejects_stale_revision_and_unpaired_plan_authority() {
    let store = SessionExecutionStore::new(initialized());
    let seeded = store
        .seed(SESSION, CollaborationMode::Work, None, 10)
        .expect("seed");
    let mut invalid = seeded.clone();
    invalid.authorized_plan_id = Some("plan_1".to_owned());
    invalid.time_updated = 20;
    let error = store
        .update(seeded.revision, invalid)
        .expect_err("unpaired plan authority");
    assert!(
        zuno_error::source::describe(&error).contains("supplied together"),
        "{error:?}"
    );

    let mut next = seeded.clone();
    next.time_updated = 30;
    let updated = store.update(seeded.revision, next).expect("update");
    let mut stale = updated;
    stale.time_updated = 40;
    let error = store.update(1, stale).expect_err("stale revision");
    assert!(
        zuno_error::source::describe(&error).contains("revision conflict"),
        "{error:?}"
    );
}
