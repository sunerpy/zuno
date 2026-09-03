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
        attachments: Vec::new(),
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

#[tokio::test]
async fn concurrent_wakes_for_one_report_queue_only_one_parent_delivery() {
    let (_pool, inbox) = initialized();
    admit(&inbox, "input_1");
    let runs = SessionRunRegistry::new();
    let guard = runs.begin_turn(SESSION).expect("active parent");
    let driver = Arc::new(RecordingDriver::new(inbox.clone()));
    let coordinator = SessionWakeCoordinator::new(inbox.clone(), runs, driver.clone());

    let first_coordinator = coordinator.clone();
    let first = tokio::spawn(async move {
        first_coordinator
            .deliver(SESSION, "input_1", message("input_1"))
            .await
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        while !guard.soft_interrupt_signal().is_set() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("first wake is queued");

    assert_eq!(
        coordinator
            .deliver(SESSION, "input_1", message("input_1"))
            .await
            .expect("duplicate wake is idempotent"),
        WakeOutcome::AlreadyInFlight
    );
    let delivery = guard.take_soft_interrupts_at_safe_point();
    assert_eq!(delivery.messages.len(), 1);
    assert_eq!(delivery.messages[0].input_id.as_deref(), Some("input_1"));
    inbox
        .promote_id(SESSION, "input_1")
        .expect("promote report")
        .expect("pending report");
    drop(guard);

    assert_eq!(
        first.await.expect("first wake task").expect("first wake"),
        WakeOutcome::ClaimedByActiveTurn
    );
    assert!(driver.calls().is_empty());
}

fn admit_prompt(inbox: &SessionInbox, id: &str) {
    inbox
        .admit(NewSessionInput::new(
            id,
            SESSION,
            json!({
                "kind": "user",
                "prompt": {"text": id, "files": [], "agents": []},
                "agent": null,
                "model": null
            }),
            InputDelivery::Queue,
            10,
        ))
        .expect("admit prompt");
}

/// A driver that claims the session's whole report batch, as the parent wake does.
#[derive(Clone)]
struct BatchDriver {
    inbox: SessionInbox,
    turns: Arc<Mutex<Vec<Vec<String>>>>,
}

impl BatchDriver {
    fn new(inbox: SessionInbox) -> Self {
        Self {
            inbox,
            turns: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn turns(&self) -> Vec<Vec<String>> {
        self.turns.lock().expect("turns lock").clone()
    }
}

#[async_trait]
impl PendingInputDriver for BatchDriver {
    async fn drive(&self, input: SessionInput, _guard: SessionRunGuard) -> Result<(), String> {
        let promoted = self
            .inbox
            .promote_pending_async(&input.session_id)
            .map_err(|error| error.to_string())?;
        self.turns
            .lock()
            .expect("turns lock")
            .push(promoted.into_iter().map(|input| input.id).collect());
        Ok(())
    }
}

#[tokio::test]
async fn one_idle_wake_drives_every_settled_report_in_a_single_turn() {
    let (_pool, inbox) = initialized();
    admit(&inbox, "input_1");
    admit(&inbox, "input_2");
    admit(&inbox, "input_3");
    let runs = SessionRunRegistry::new();
    let driver = Arc::new(BatchDriver::new(inbox.clone()));
    let coordinator = SessionWakeCoordinator::new(inbox.clone(), runs, driver.clone());

    let outcome = coordinator
        .deliver(SESSION, "input_1", message("input_1"))
        .await
        .expect("deliver report");

    assert_eq!(outcome, WakeOutcome::Driven);
    assert_eq!(
        driver.turns(),
        [vec![
            "input_1".to_owned(),
            "input_2".to_owned(),
            "input_3".to_owned()
        ]],
        "three settled reports must cost one turn, not three"
    );
    assert!(inbox.pending(SESSION).expect("pending").is_empty());
}

#[tokio::test]
async fn a_busy_parent_is_offered_the_whole_report_batch_at_one_safe_point() {
    let (_pool, inbox) = initialized();
    admit(&inbox, "input_1");
    admit(&inbox, "input_2");
    admit(&inbox, "input_3");
    let runs = SessionRunRegistry::new();
    let guard = runs.begin_turn(SESSION).expect("active parent");
    let driver = Arc::new(BatchDriver::new(inbox.clone()));
    let coordinator = SessionWakeCoordinator::new(inbox.clone(), runs, driver.clone());
    let delivery = tokio::spawn(async move {
        coordinator
            .deliver(SESSION, "input_1", message("input_1"))
            .await
    });

    let queued = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let delivery = guard.take_soft_interrupts_at_safe_point();
            if delivery.messages.len() == 3 {
                break delivery.messages;
            }
            assert!(
                delivery.messages.is_empty(),
                "a partial batch would split one settled batch across turns"
            );
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the whole batch arrives at one safe point");

    assert_eq!(
        queued
            .iter()
            .map(|message| message.input_id.clone().expect("durable input id"))
            .collect::<Vec<_>>(),
        ["input_1", "input_2", "input_3"]
    );
    assert_eq!(
        queued
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>(),
        ["input_1", "input_2", "input_3"],
        "every batch member is steered from its own durable row, so the running turn \
         reads exactly what an idle turn would have driven"
    );
    inbox
        .promote_pending_async(SESSION)
        .expect("the active turn claims the batch");
    drop(guard);

    assert_eq!(
        delivery.await.expect("delivery task").expect("delivery"),
        WakeOutcome::ClaimedByActiveTurn
    );
    assert!(driver.turns().is_empty());
}

#[tokio::test]
async fn a_typed_submission_wake_never_drags_settled_reports_into_the_running_turn() {
    let (_pool, inbox) = initialized();
    admit_prompt(&inbox, "typed");
    admit(&inbox, "input_1");
    let runs = SessionRunRegistry::new();
    let guard = runs.begin_turn(SESSION).expect("active parent");
    let driver = Arc::new(BatchDriver::new(inbox.clone()));
    let coordinator = SessionWakeCoordinator::new(inbox.clone(), runs, driver.clone());
    let delivery = tokio::spawn(async move {
        coordinator
            .deliver(SESSION, "typed", message("typed"))
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
    .expect("the typed submission arrives");

    assert_eq!(queued.input_id.as_deref(), Some("typed"));
    assert!(
        guard
            .take_soft_interrupts_at_safe_point()
            .messages
            .is_empty(),
        "only the report wake batches; a typed submission steers exactly one row"
    );
    inbox
        .promote_id(SESSION, "typed")
        .expect("promote submission")
        .expect("pending submission");
    drop(guard);

    assert_eq!(
        delivery.await.expect("delivery task").expect("delivery"),
        WakeOutcome::ClaimedByActiveTurn
    );
    assert_eq!(
        inbox
            .pending(SESSION)
            .expect("pending")
            .into_iter()
            .map(|input| input.id)
            .collect::<Vec<_>>(),
        ["input_1"]
    );
}

fn admit_job_report(inbox: &SessionInbox, id: &str, job_id: &str, text: &str, completed: i64) {
    inbox
        .admit(NewSessionInput::new(
            id,
            SESSION,
            json!({
                "kind": "subagentReport",
                "jobID": job_id,
                "childSessionID": "ses_child",
                "status": "completed",
                "text": text
            }),
            InputDelivery::Queue,
            completed,
        ))
        .expect("admit report");
}

#[tokio::test]
async fn a_busy_parent_reads_a_superseded_report_as_superseded() {
    let (_pool, inbox) = initialized();
    admit_job_report(&inbox, "input_early", "job_1", "child is halfway", 10);
    admit_job_report(&inbox, "input_late", "job_1", "child finished", 20);
    let runs = SessionRunRegistry::new();
    let guard = runs.begin_turn(SESSION).expect("active parent");
    let driver = Arc::new(BatchDriver::new(inbox.clone()));
    let coordinator = SessionWakeCoordinator::new(inbox.clone(), runs, driver.clone());
    let delivery = tokio::spawn(async move {
        coordinator
            .deliver(SESSION, "input_early", message("input_early"))
            .await
    });

    let queued = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let delivery = guard.take_soft_interrupts_at_safe_point();
            if delivery.messages.len() == 2 {
                break delivery.messages;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("both reports for one job arrive together");

    assert_eq!(
        queued[0].input_id.as_deref(),
        Some("input_early"),
        "the batch keeps admission order"
    );
    assert!(
        queued[0].content.starts_with("[superseded report]"),
        "the running turn must not read a replaced state as live: {}",
        queued[0].content
    );
    assert!(
        queued[0].content.ends_with("child is halfway"),
        "annotation is a prefix on the durable text, never a rewrite: {}",
        queued[0].content
    );
    assert!(
        queued[1].content.starts_with("[current report]"),
        "the newest report for the job is the one marked current: {}",
        queued[1].content
    );
    assert!(queued[1].content.ends_with("child finished"));
    inbox
        .promote_pending_async(SESSION)
        .expect("the active turn claims the batch");
    drop(guard);

    assert_eq!(
        delivery.await.expect("delivery task").expect("delivery"),
        WakeOutcome::ClaimedByActiveTurn
    );
}

#[tokio::test]
async fn a_report_the_projection_cannot_render_still_steers_its_own_wake() {
    let (_pool, inbox) = initialized();
    inbox
        .admit(NewSessionInput::new(
            "input_broken",
            SESSION,
            json!({"kind": "subagentReport", "jobID": "job_1", "status": "completed"}),
            InputDelivery::Queue,
            10,
        ))
        .expect("admit report");
    let runs = SessionRunRegistry::new();
    let guard = runs.begin_turn(SESSION).expect("active parent");
    let driver = Arc::new(BatchDriver::new(inbox.clone()));
    let coordinator = SessionWakeCoordinator::new(inbox.clone(), runs, driver.clone());
    let delivery = tokio::spawn(async move {
        coordinator
            .deliver(SESSION, "input_broken", message("input_broken"))
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
    .expect("the wake still reaches the running turn");

    assert_eq!(queued.input_id.as_deref(), Some("input_broken"));
    assert_eq!(
        queued.content, "report input_broken",
        "a row the projection drops is offered exactly as its caller built it"
    );
    inbox
        .promote_id(SESSION, "input_broken")
        .expect("promote report")
        .expect("pending report");
    drop(guard);

    assert_eq!(
        delivery.await.expect("delivery task").expect("delivery"),
        WakeOutcome::ClaimedByActiveTurn
    );
}
