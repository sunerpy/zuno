//! Runtime-store behavior. Input consumption stands in for the kernel's
//! materialization transaction; these tests are not remote-worker certification.

use std::num::{NonZeroU64, NonZeroUsize};
use std::sync::Arc;

use serde_json::json;
use zuno_application::ApplicationError;
use zuno_application::runtime::{
    ConfigurationRef, JobFinish, JobPhase, JobSubmission, LeaseDuration, RuntimeCheckpoint,
    RuntimeJob, RuntimeStore,
};
use zuno_db::event_log::SessionEventLog;
use zuno_db::inbox::SessionInbox;
use zuno_db::runtime_store::SqliteRuntimeStore;
use zuno_db::session::{SessionCreate, Store};
use zuno_db::{Pool, migration};
use zuno_paths::DbLocation;
use zuno_types::identity::{
    ClientId, ConfigurationId, PrincipalId, PrincipalKind, PrincipalScope, RequestId, SessionId,
    TenantId, WorkerInstanceId,
};

struct Fixture {
    _directory: tempfile::TempDir,
    pool: Arc<Pool>,
    store: SqliteRuntimeStore,
    alice: PrincipalScope,
    bob: PrincipalScope,
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
    let principal = |name| {
        PrincipalScope::new(
            TenantId::new("organization").unwrap(),
            PrincipalId::new(name).unwrap(),
            PrincipalKind::User,
            Some(ClientId::new("web").unwrap()),
            NonZeroU64::MIN,
        )
    };
    let (alice, bob) = (principal("alice"), principal("bob"));
    for (id, owner) in [
        ("ses_a", alice.owner()),
        ("ses_a2", alice.owner()),
        ("ses_b", bob.owner()),
    ] {
        Store::new(&pool)
            .create(
                &SessionCreate::new(
                    id,
                    id,
                    "project",
                    directory.path().to_str().unwrap(),
                    directory.path().to_str().unwrap(),
                    "Runtime task",
                    "preview",
                )
                .with_owner(owner),
            )
            .unwrap();
    }
    Fixture {
        store: SqliteRuntimeStore::new(pool.clone(), NonZeroUsize::new(4).unwrap()).unwrap(),
        pool,
        alice,
        bob,
        _directory: directory,
    }
}

fn submission(session: &str, request: &str, version: u64) -> JobSubmission {
    JobSubmission {
        session_id: SessionId::new(session).unwrap(),
        request_id: RequestId::new(request).unwrap(),
        expected_input_version: version,
        text: format!("Work on {request}"),
        configuration: ConfigurationRef {
            id: ConfigurationId::new("definition").unwrap(),
            version: 1,
            sha256: "a".repeat(64),
        },
    }
}

fn worker(id: &str) -> WorkerInstanceId {
    WorkerInstanceId::new(id).unwrap()
}
fn duration() -> LeaseDuration {
    LeaseDuration::new(30_000).unwrap()
}

fn consume(fixture: &Fixture, job: &RuntimeJob) {
    let inbox = SessionInbox::new(fixture.pool.clone());
    inbox
        .promote_id(job.session_id.as_str(), job.input_id.as_str())
        .unwrap()
        .unwrap();
    inbox
        .mark_consumed(job.session_id.as_str(), job.input_id.as_str())
        .unwrap()
        .unwrap();
}

fn checkpoint(job: &RuntimeJob) -> RuntimeCheckpoint {
    RuntimeCheckpoint {
        job_id: job.id.clone(),
        session_id: job.session_id.clone(),
        turn_id: job.turn_id.clone(),
        driver: "default".to_owned(),
        schema_version: 2,
        reference: json!({"eventId":"fixture-boundary","sequence":1}),
    }
}

#[tokio::test]
async fn a_valid_lease_cannot_be_routed_through_another_owner() {
    let fixture = fixture();
    fixture
        .store
        .submit(&fixture.alice, submission("ses_a", "owner-fence", 0))
        .await
        .unwrap();
    let claimed = fixture
        .store
        .claim(&worker("worker"), duration())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.lease.owner, fixture.alice.owner());
    let mut forged = claimed.lease.clone();
    forged.owner = fixture.bob.owner();
    assert!(matches!(
        fixture.store.renew(&forged, duration()).await,
        Err(ApplicationError::LeaseLost)
    ));
    assert!(matches!(
        fixture
            .store
            .finish(
                &forged,
                JobFinish::Cancelled {
                    reason: "forged owner".to_owned()
                }
            )
            .await,
        Err(ApplicationError::LeaseLost)
    ));
    fixture
        .store
        .renew(&claimed.lease, duration())
        .await
        .unwrap();
    assert_eq!(
        fixture
            .store
            .get(&fixture.alice.owner(), &claimed.job.id)
            .await
            .unwrap()
            .phase,
        JobPhase::Running
    );
}

#[tokio::test]
async fn admission_is_atomic_idempotent_and_checks_the_separate_input_version() {
    let fixture = fixture();
    let request = submission("ses_a", "request", 0);
    let runtime = zuno_runtime::HarnessRuntime::new("runtime-contract");
    runtime
        .mount(zuno_application::runtime::JobDispatcher::new(Arc::new(
            fixture.store.clone(),
        )))
        .await
        .unwrap();
    let dispatcher = runtime
        .service::<zuno_application::runtime::JobDispatcher>()
        .unwrap();
    let (first, repeated) = tokio::join!(
        dispatcher.dispatch(&fixture.alice, request.clone()),
        dispatcher.dispatch(&fixture.alice, request.clone()),
    );
    let first = first.unwrap();
    assert_eq!(first, repeated.unwrap());
    assert_eq!(first.input_version, 1);
    let mut changed = request;
    changed.text = "changed request".to_owned();
    assert!(matches!(
        fixture.store.submit(&fixture.alice, changed).await,
        Err(ApplicationError::Conflict)
    ));
    assert!(matches!(
        fixture
            .store
            .submit(&fixture.alice, submission("ses_a", "second", 0))
            .await,
        Err(ApplicationError::Conflict)
    ));
    let second = fixture
        .store
        .submit(&fixture.alice, submission("ses_a", "second", 1))
        .await
        .unwrap();
    assert_eq!(second.input_version, 2);
    assert!(matches!(
        fixture.store.get(&fixture.bob.owner(), &first.id).await,
        Err(ApplicationError::NotFound)
    ));
    let inbox = SessionInbox::new(fixture.pool.clone())
        .pending("ses_a")
        .unwrap();
    assert_eq!(inbox.len(), 2);
    assert_eq!(inbox[0].id, first.input_id.as_str());
}

#[tokio::test]
async fn failed_runtime_audit_rolls_back_input_job_clock_and_every_related_row() {
    let fixture = fixture();
    fixture
        .pool
        .get()
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER fail_runtime_admit BEFORE INSERT ON event
         WHEN NEW.type LIKE 'runtime.job.admitted%'
         BEGIN SELECT RAISE(ABORT,'injected runtime audit failure'); END;",
        )
        .unwrap();
    assert!(
        fixture
            .store
            .submit(&fixture.alice, submission("ses_a", "atomic", 0))
            .await
            .is_err()
    );
    assert_eq!(
        fixture
            .store
            .input_version(&fixture.alice.owner(), &SessionId::new("ses_a").unwrap())
            .await
            .unwrap(),
        0
    );
    let connection = fixture.pool.get().unwrap();
    for table in [
        "agent_job",
        "runtime_job",
        "session_input",
        "event",
        "event_sequence",
        "runtime_owner_schedule",
    ] {
        let count: i64 = connection
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 0, "{table} is part of admission");
    }
}

#[tokio::test]
async fn two_workers_cannot_own_one_session_and_a_checkpoint_keeps_its_logical_turn() {
    let fixture = fixture();
    let first = fixture
        .store
        .submit(&fixture.alice, submission("ses_a", "first", 0))
        .await
        .unwrap();
    let second = fixture
        .store
        .submit(&fixture.alice, submission("ses_a", "second", 1))
        .await
        .unwrap();
    let worker_a = worker("worker-a");
    let worker_b = worker("worker-b");
    let (left, right) = tokio::join!(
        fixture.store.claim(&worker_a, duration()),
        fixture.store.claim(&worker_b, duration()),
    );
    let claims = [left.unwrap(), right.unwrap()]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    assert_eq!(claims.len(), 1);
    let first_claim = &claims[0];
    assert_eq!(first_claim.job.id, first.id);
    assert!(
        fixture
            .store
            .checkpoint(&first_claim.lease, checkpoint(&first))
            .await
            .is_err(),
        "a checkpoint cannot pretend an input was model-visible"
    );
    consume(&fixture, &first);
    let saved = fixture
        .store
        .checkpoint(&first_claim.lease, checkpoint(&first))
        .await
        .unwrap();
    assert_eq!(saved.checkpoint_version, 1);
    assert_eq!(saved.phase, JobPhase::Ready);
    assert_eq!(
        fixture
            .store
            .input_version(&fixture.alice.owner(), &first.session_id)
            .await
            .unwrap(),
        2,
        "promotion/consumption does not mutate the input admission clock"
    );
    let resumed = fixture
        .store
        .claim(&worker("worker-c"), duration())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        resumed.job.id, first.id,
        "a queued second turn cannot overtake a checkpointed first turn"
    );
    assert_eq!(resumed.job.turn_id, first.turn_id);
    assert!(resumed.lease.epoch > first_claim.lease.epoch);
    assert_ne!(resumed.lease.attempt_id, first_claim.lease.attempt_id);
    assert!(matches!(
        fixture
            .store
            .finish(
                &first_claim.lease,
                JobFinish::Completed { result: json!({}) }
            )
            .await,
        Err(ApplicationError::LeaseLost)
    ));
    fixture
        .store
        .finish(
            &resumed.lease,
            JobFinish::Completed {
                result: json!({"answer":"done"}),
            },
        )
        .await
        .unwrap();
    let next = fixture
        .store
        .claim(&worker("worker-d"), duration())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(next.job.id, second.id);
    assert!(
        SessionInbox::new(fixture.pool.clone())
            .pending("ses_a")
            .unwrap()
            .iter()
            .all(|input| input.prompt["kind"] == "user"),
        "a root completion does not report back to itself"
    );
}

#[tokio::test]
async fn an_expired_worker_is_fenced_and_its_uncertain_session_does_not_block_other_users() {
    let fixture = fixture();
    let first = fixture
        .store
        .submit(&fixture.alice, submission("ses_a", "first", 0))
        .await
        .unwrap();
    let claimed = fixture
        .store
        .claim(&worker("lost-worker"), duration())
        .await
        .unwrap()
        .unwrap();
    fixture
        .store
        .submit(&fixture.alice, submission("ses_a", "second", 1))
        .await
        .unwrap();
    let other = fixture
        .store
        .submit(&fixture.bob, submission("ses_b", "other", 0))
        .await
        .unwrap();
    fixture
        .pool
        .get()
        .unwrap()
        .execute(
            "UPDATE runtime_session SET lease_expires=1 WHERE session_id='ses_a'",
            [],
        )
        .unwrap();
    assert!(matches!(
        fixture.store.renew(&claimed.lease, duration()).await,
        Err(ApplicationError::LeaseLost)
    ));
    let next = fixture
        .store
        .claim(&worker("replacement"), duration())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(next.job.id, other.id);
    assert_eq!(
        fixture
            .store
            .get(&fixture.alice.owner(), &first.id)
            .await
            .unwrap()
            .phase,
        JobPhase::Uncertain
    );
    assert!(matches!(
        fixture
            .store
            .finish(&claimed.lease, JobFinish::Completed { result: json!({}) })
            .await,
        Err(ApplicationError::LeaseLost)
    ));
    assert!(
        fixture
            .store
            .claim(&worker("another"), duration())
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn checkpoint_identity_and_direct_native_transitions_cannot_bypass_authority() {
    let fixture = fixture();
    let job = fixture
        .store
        .submit(&fixture.alice, submission("ses_a", "first", 0))
        .await
        .unwrap();
    assert!(
        zuno_db::job::AgentJobStore::new(fixture.pool.clone())
            .start(job.id.as_str(), 1)
            .is_err()
    );
    let claimed = fixture
        .store
        .claim(&worker("worker"), duration())
        .await
        .unwrap()
        .unwrap();
    consume(&fixture, &job);
    let mut wrong = checkpoint(&job);
    wrong.session_id = SessionId::new("ses_b").unwrap();
    assert!(matches!(
        fixture.store.checkpoint(&claimed.lease, wrong).await,
        Err(ApplicationError::Conflict)
    ));
    assert!(
        zuno_db::job::AgentJobStore::new(fixture.pool.clone())
            .settle(
                job.id.as_str(),
                zuno_db::job::JobSettlement::completed(json!({}), 2, None),
            )
            .is_err()
    );
    assert_eq!(
        fixture
            .store
            .get(&fixture.alice.owner(), &job.id)
            .await
            .unwrap()
            .phase,
        JobPhase::Running
    );
}

#[tokio::test]
async fn ready_checkpoints_yield_capacity_to_another_owner() {
    let fixture = fixture();
    let a = fixture
        .store
        .submit(&fixture.alice, submission("ses_a", "a", 0))
        .await
        .unwrap();
    let first = fixture
        .store
        .claim(&worker("worker"), duration())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.job.id, a.id);
    consume(&fixture, &a);
    fixture
        .store
        .checkpoint(&first.lease, checkpoint(&a))
        .await
        .unwrap();
    let b = fixture
        .store
        .submit(&fixture.bob, submission("ses_b", "b", 0))
        .await
        .unwrap();
    fixture
        .store
        .submit(&fixture.alice, submission("ses_a2", "a2", 0))
        .await
        .unwrap();
    let next = fixture
        .store
        .claim(&worker("worker"), duration())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        next.job.id, b.id,
        "a busy owner cannot monopolize every released slot"
    );
    let events = SessionEventLog::new(fixture.pool.clone())
        .read_after(a.session_id.as_str(), None)
        .unwrap();
    assert!(
        events
            .iter()
            .any(|event| event.event_type == "runtime.checkpoint.committed")
    );
}
