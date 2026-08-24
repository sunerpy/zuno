use async_trait::async_trait;
use serde_json::json;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use zuno_db::inbox::{InputDelivery, NewSessionInput, SessionInbox, SessionInput};
use zuno_db::{Pool, migration, session};
use zuno_engine::interrupt::{SoftInterruptMessage, SoftInterruptSource};
use zuno_engine::status::{SessionRunGuard, SessionRunRegistry};
use zuno_engine::wake::{PendingInputDriver, SessionWakeCoordinator, WakeOutcome};
use zuno_paths::DbLocation;

const SESSION: &str = "ses_parent";

fn initialized() -> (Arc<Pool>, SessionInbox) {
    let pool = Arc::new(Pool::open(&DbLocation::Memory).expect("open database"));
    {
        let mut connection = pool.get().expect("database connection");
        migration::apply(&mut connection).expect("apply schema");
        connection
            .execute(
                "INSERT INTO project \
                 (id, worktree, time_created, time_updated, sandboxes) \
                 VALUES ('project', '/workspace', 1, 1, '[]')",
                [],
            )
            .expect("create project");
    }
    pool.transaction(|transaction| {
        session::create(
            transaction,
            &session::SessionCreate::new(
                SESSION,
                "parent",
                "project",
                "/workspace",
                "/workspace",
                "Parent",
                "zuno",
            )
            .at(1),
        )
        .map(|_| ())
    })
    .expect("create session");
    let inbox = SessionInbox::new(Arc::clone(&pool));
    (pool, inbox)
}

fn admit(inbox: &SessionInbox, id: &str) {
    inbox
        .admit(NewSessionInput::new(
            id,
            SESSION,
            json!({"kind": "subagentReport", "text": id}),
            InputDelivery::Queue,
            10,
        ))
        .expect("admit input");
}

fn message(id: &str) -> SoftInterruptMessage {
    SoftInterruptMessage {
        input_id: Some(id.to_owned()),
        content: format!("report {id}"),
        images: Vec::new(),
        urgent: false,
        source: SoftInterruptSource::BackgroundTask,
    }
}

#[derive(Clone)]
struct RecordingDriver {
    inbox: SessionInbox,
    calls: Arc<Mutex<Vec<String>>>,
}

impl RecordingDriver {
    fn new(inbox: SessionInbox) -> Self {
        Self {
            inbox,
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().expect("calls lock").clone()
    }
}

#[async_trait]
impl PendingInputDriver for RecordingDriver {
    async fn drive(&self, input: SessionInput, _guard: SessionRunGuard) -> Result<(), String> {
        self.inbox
            .promote_id(&input.session_id, &input.id)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "driver received an input that was no longer pending".to_owned())?;
        self.calls.lock().expect("calls lock").push(input.id);
        Ok(())
    }
}

#[tokio::test]
async fn an_idle_parent_is_driven_and_the_report_is_claimed_once() {
    let (_pool, inbox) = initialized();
    admit(&inbox, "input_1");
    let runs = SessionRunRegistry::new();
    let driver = Arc::new(RecordingDriver::new(inbox.clone()));
    let coordinator = SessionWakeCoordinator::new(inbox.clone(), runs, driver.clone());

    let outcome = coordinator
        .deliver(SESSION, "input_1", message("input_1"))
        .await
        .expect("deliver report");

    assert_eq!(outcome, WakeOutcome::Driven);
    assert_eq!(driver.calls(), ["input_1"]);
    assert!(inbox.pending(SESSION).expect("pending").is_empty());
}

#[tokio::test]
async fn an_active_parent_claims_the_report_at_its_safe_point() {
    let (_pool, inbox) = initialized();
    admit(&inbox, "input_1");
    let runs = SessionRunRegistry::new();
    let guard = runs.begin_turn(SESSION).expect("active parent");
    let driver = Arc::new(RecordingDriver::new(inbox.clone()));
    let coordinator = SessionWakeCoordinator::new(inbox.clone(), runs, driver.clone());
    let delivery = tokio::spawn(async move {
        coordinator
            .deliver(SESSION, "input_1", message("input_1"))
            .await
    });

    let queued = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let delivery = guard.take_soft_interrupts_at_safe_point();
            if let Some(message) = delivery.messages.into_iter().next() {
                break message;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("soft interrupt arrives");
    assert_eq!(queued.input_id.as_deref(), Some("input_1"));
    inbox
        .promote_id(SESSION, "input_1")
        .expect("promote report")
        .expect("pending report");
    drop(guard);

    assert_eq!(
        delivery.await.expect("delivery task").expect("delivery"),
        WakeOutcome::ClaimedByActiveTurn
    );
    assert!(driver.calls().is_empty());
}

#[tokio::test]
async fn a_report_queued_after_the_last_safe_point_starts_the_next_turn() {
    let (_pool, inbox) = initialized();
    admit(&inbox, "input_1");
    let runs = SessionRunRegistry::new();
    let guard = runs.begin_turn(SESSION).expect("active parent");
    let driver = Arc::new(RecordingDriver::new(inbox.clone()));
    let coordinator = SessionWakeCoordinator::new(inbox.clone(), runs, driver.clone());
    let delivery = tokio::spawn(async move {
        coordinator
            .deliver(SESSION, "input_1", message("input_1"))
            .await
    });

    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if !guard
                .take_soft_interrupts_at_safe_point()
                .messages
                .is_empty()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("soft interrupt was accepted");
    drop(guard);

    assert_eq!(
        delivery.await.expect("delivery task").expect("delivery"),
        WakeOutcome::Driven
    );
    assert_eq!(driver.calls(), ["input_1"]);
    assert!(inbox.pending(SESSION).expect("pending").is_empty());
}
