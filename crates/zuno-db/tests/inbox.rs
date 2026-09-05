use serde_json::json;
use std::sync::Arc;
use zuno_db::event_log::{NewSessionEvent, SessionEventLog};
use zuno_db::inbox::{
    DurableInputKind, InputDelivery, NewSessionInput, SessionInbox, SubmissionState,
};
use zuno_db::{Pool, migration, session};
use zuno_paths::DbLocation;

const SESSION_ID: &str = "ses_inbox";

fn initialized(location: &DbLocation) -> Arc<Pool> {
    let pool = Arc::new(Pool::open(location).expect("open database"));
    {
        let mut connection = pool.get().expect("database connection");
        migration::apply(&mut connection).expect("apply schema");
        connection
            .execute(
                "INSERT OR IGNORE INTO project \
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
                SESSION_ID,
                "inbox",
                "project",
                "/workspace",
                "/workspace",
                "Inbox test",
                "zuno",
            )
            .at(1),
        )
        .map(|_| ())
    })
    .expect("create session");
    pool
}

fn input(id: &str, text: &str, delivery: InputDelivery, time: i64) -> NewSessionInput {
    NewSessionInput::new(id, SESSION_ID, json!({"text": text}), delivery, time)
}

#[test]
fn admission_commits_the_event_and_pending_input_together() {
    let pool = initialized(&DbLocation::Memory);
    let inbox = SessionInbox::new(Arc::clone(&pool));
    let log = SessionEventLog::new(pool);

    let admitted = inbox
        .admit(input("input-1", "first", InputDelivery::Queue, 10))
        .expect("admit input");

    assert_eq!(admitted.state, SubmissionState::Queued);
    assert_eq!(admitted.revision, 1);
    assert_eq!(admitted.admitted_sequence, 0);
    assert_eq!(admitted.promoted_sequence, None);
    assert_eq!(admitted.time_updated, admitted.time_created);
    assert_eq!(inbox.pending(SESSION_ID).expect("pending"), [admitted]);
    let events = log.read_after(SESSION_ID, None).expect("events");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event_type, "session.input.admitted");
    assert_eq!(events[0].properties["inputID"], "input-1");
}

#[test]
fn promotion_is_fifo_and_each_input_is_claimed_once() {
    let pool = initialized(&DbLocation::Memory);
    let inbox = SessionInbox::new(pool);
    inbox
        .admit(input("input-1", "first", InputDelivery::Queue, 10))
        .expect("first admission");
    inbox
        .admit(input("input-2", "second", InputDelivery::Queue, 11))
        .expect("second admission");

    let first = inbox
        .promote_next(SESSION_ID, None)
        .expect("first promotion")
        .expect("first pending input");
    let second = inbox
        .promote_next(SESSION_ID, None)
        .expect("second promotion")
        .expect("second pending input");

    assert_eq!(first.id, "input-1");
    assert_eq!(second.id, "input-2");
    assert!(first.promoted_sequence.is_some());
    assert!(second.promoted_sequence > first.promoted_sequence);
    assert_eq!(
        inbox
            .promote_next(SESSION_ID, None)
            .expect("empty promotion"),
        None
    );
}

#[test]
fn delivery_filter_promotes_only_the_requested_class() {
    let pool = initialized(&DbLocation::Memory);
    let inbox = SessionInbox::new(pool);
    inbox
        .admit(input("steer", "urgent", InputDelivery::Steer, 10))
        .expect("steer admission");
    inbox
        .admit(input("next", "later", InputDelivery::Queue, 11))
        .expect("next-step admission");

    let promoted = inbox
        .promote_next(SESSION_ID, Some(InputDelivery::Queue))
        .expect("promotion")
        .expect("next-step input");

    assert_eq!(promoted.id, "next");
    assert_eq!(
        inbox
            .pending(SESSION_ID)
            .expect("pending")
            .into_iter()
            .map(|input| input.id)
            .collect::<Vec<_>>(),
        ["steer"]
    );
}

#[test]
fn promotion_by_id_claims_only_the_live_injected_input() {
    let pool = initialized(&DbLocation::Memory);
    let inbox = SessionInbox::new(pool);
    inbox
        .admit(input("first", "first", InputDelivery::Queue, 10))
        .expect("first admission");
    inbox
        .admit(input("steered", "now", InputDelivery::Steer, 11))
        .expect("steer admission");

    let promoted = inbox
        .promote_id(SESSION_ID, "steered")
        .expect("promotion")
        .expect("pending steer");

    assert_eq!(promoted.id, "steered");
    assert_eq!(
        inbox
            .pending(SESSION_ID)
            .expect("pending")
            .into_iter()
            .map(|input| input.id)
            .collect::<Vec<_>>(),
        ["first"]
    );
    assert_eq!(
        inbox
            .promote_id(SESSION_ID, "steered")
            .expect("repeat promotion"),
        None
    );
}

#[test]
fn two_concurrent_promoters_cannot_claim_the_same_input() {
    let pool = initialized(&DbLocation::Memory);
    let inbox = SessionInbox::new(pool);
    inbox
        .admit(input("input-1", "only", InputDelivery::Queue, 10))
        .expect("admission");

    let left = {
        let inbox = inbox.clone();
        std::thread::spawn(move || {
            inbox
                .promote_next(SESSION_ID, None)
                .expect("left promotion")
        })
    };
    let right = {
        let inbox = inbox.clone();
        std::thread::spawn(move || {
            inbox
                .promote_next(SESSION_ID, None)
                .expect("right promotion")
        })
    };
    let claimed = [
        left.join().expect("left thread"),
        right.join().expect("right thread"),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();

    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].id, "input-1");
}

#[test]
fn pending_inputs_survive_pool_and_process_reconstruction() {
    let directory = tempfile::tempdir().expect("temporary database directory");
    let location = DbLocation::File(directory.path().join("zuno.db"));
    {
        let pool = initialized(&location);
        SessionInbox::new(pool)
            .admit(input("input-1", "recover me", InputDelivery::Queue, 10))
            .expect("admission");
    }

    let recovered = SessionInbox::new(initialized(&location))
        .pending(SESSION_ID)
        .expect("recovered pending inputs");

    assert_eq!(recovered.len(), 1);
    assert_eq!(recovered[0].prompt["text"], "recover me");
}

#[test]
fn failed_admission_rolls_back_its_event_sequence() {
    let pool = initialized(&DbLocation::Memory);
    let inbox = SessionInbox::new(Arc::clone(&pool));
    let log = SessionEventLog::new(pool);
    inbox
        .admit(input("duplicate", "first", InputDelivery::Queue, 10))
        .expect("first admission");

    inbox
        .admit(input("duplicate", "second", InputDelivery::Queue, 11))
        .expect_err("duplicate id must fail");
    log.append(
        SESSION_ID,
        NewSessionEvent::new("test.after.failure", serde_json::Map::new()).expect("valid event"),
    )
    .expect("append after failed admission");

    let events = log.read_after(SESSION_ID, None).expect("events");
    assert_eq!(
        events
            .iter()
            .map(|event| event.sequence)
            .collect::<Vec<_>>(),
        [0, 1],
        "the rolled-back admission must not consume sequence 1"
    );
    assert_eq!(events[1].event_type, "test.after.failure");
}

#[test]
fn pending_inputs_can_be_edited_and_cancelled_with_revisions() {
    let pool = initialized(&DbLocation::Memory);
    let inbox = SessionInbox::new(Arc::clone(&pool));
    let log = SessionEventLog::new(pool);
    inbox
        .admit(input("input-1", "first", InputDelivery::Queue, 10))
        .expect("admission");

    let edited = inbox
        .edit_pending(SESSION_ID, "input-1", 1, json!({"text": "revised"}), 20)
        .expect("edit pending input");
    assert_eq!(edited.state, SubmissionState::Queued);
    assert_eq!(edited.revision, 2);
    assert_eq!(edited.prompt["text"], "revised");
    assert_eq!(edited.time_updated, 20);

    let conflict = inbox
        .edit_pending(SESSION_ID, "input-1", 1, json!({"text": "stale"}), 21)
        .expect_err("stale edit must fail");
    assert!(conflict.to_string().contains("revision conflict"));

    let cancelled = inbox
        .cancel_pending(SESSION_ID, "input-1", 2, 30)
        .expect("cancel pending input");
    assert_eq!(cancelled.state, SubmissionState::Cancelled);
    assert_eq!(cancelled.revision, 3);
    assert!(inbox.pending(SESSION_ID).expect("pending").is_empty());
    assert_eq!(
        inbox
            .get(SESSION_ID, "input-1")
            .expect("stored input")
            .expect("input row")
            .state,
        SubmissionState::Cancelled
    );
    assert_eq!(
        log.read_after(SESSION_ID, None)
            .expect("events")
            .into_iter()
            .map(|event| event.event_type)
            .collect::<Vec<_>>(),
        [
            "session.input.admitted",
            "session.input.edited",
            "session.input.cancelled",
        ],
        "the rejected stale edit must not consume an event sequence"
    );
}

#[test]
fn promoted_inputs_settle_consumed_once() {
    let pool = initialized(&DbLocation::Memory);
    let inbox = SessionInbox::new(pool);
    inbox
        .admit(input("input-1", "first", InputDelivery::Queue, 10))
        .expect("admission");

    let promoted = inbox
        .promote_next(SESSION_ID, None)
        .expect("promotion")
        .expect("promoted input");
    assert_eq!(promoted.state, SubmissionState::Promoted);
    assert_eq!(promoted.revision, 2);

    let consumed = inbox
        .mark_consumed(SESSION_ID, "input-1")
        .expect("consume input")
        .expect("consumed input");
    assert_eq!(consumed.state, SubmissionState::Consumed);
    assert_eq!(consumed.revision, 3);
    assert_eq!(
        inbox
            .mark_consumed(SESSION_ID, "input-1")
            .expect("repeat consume"),
        None,
        "a consumed input cannot be settled twice"
    );
}

#[test]
fn failed_inputs_leave_the_pending_queue_with_a_diagnostic() {
    let pool = initialized(&DbLocation::Memory);
    let inbox = SessionInbox::new(pool);
    let admitted = inbox
        .admit(input("input-1", "urgent", InputDelivery::Steer, 10))
        .expect("admission");
    assert_eq!(admitted.state, SubmissionState::Steering);

    let failed = inbox
        .mark_failed(SESSION_ID, "input-1", "invalid persisted prompt")
        .expect("mark failed")
        .expect("failed input");
    assert_eq!(failed.state, SubmissionState::Failed);
    assert_eq!(failed.error.as_deref(), Some("invalid persisted prompt"));
    assert!(inbox.pending(SESSION_ID).expect("pending").is_empty());
    assert!(
        inbox
            .edit_pending(
                SESSION_ID,
                "input-1",
                failed.revision,
                json!({"text": "too late"}),
                20,
            )
            .expect_err("failed input is immutable")
            .to_string()
            .contains("already failed")
    );
}

/// Every published `session_input.prompt` shape, with the keys its writer emits.
///
/// A driver that cannot run a shape must be able to recognize and skip it. Deriving
/// that from ad-hoc `kind` string tests is what let one foreign row fail a surface
/// permanently, so the classification is pinned here against the real payloads.
fn published_prompt_shapes() -> Vec<(DurableInputKind, serde_json::Value)> {
    vec![
        (
            DurableInputKind::TuiPrompt,
            json!({
                "kind": "tuiPrompt",
                "submission": {"kind": "text", "data": "hello"},
                "origin": "tui_keybinding"
            }),
        ),
        (
            DurableInputKind::AcpPrompt,
            json!({
                "kind": "acpPrompt",
                "text": "hello",
                "content": [{"type": "text", "text": "hello"}]
            }),
        ),
        (
            DurableInputKind::User,
            json!({
                "kind": "user",
                "prompt": {"text": "hello", "files": [], "agents": []},
                "agent": null,
                "model": null
            }),
        ),
        (
            DurableInputKind::SubagentReport,
            json!({
                "kind": "subagentReport",
                "jobID": "job_1",
                "childSessionID": "ses_child",
                "status": "completed",
                "text": "done"
            }),
        ),
        (
            DurableInputKind::ProductAgentReport,
            json!({
                "kind": "productAgentReport",
                "jobID": "job_1",
                "runID": "run_1",
                "product": "review",
                "instance": "default",
                "tool": "review_run",
                "status": "completed",
                "text": "done"
            }),
        ),
        (
            DurableInputKind::WorkflowReport,
            json!({
                "kind": "workflowReport",
                "jobID": "job_1",
                "runID": "run_1",
                "workflow": "release",
                "status": "completed",
                "text": "done"
            }),
        ),
        (
            DurableInputKind::CouncilReport,
            json!({
                "kind": "councilReport",
                "jobID": "job_1",
                "runID": "run_1",
                "preset": "review",
                "status": "completed",
                "text": "done"
            }),
        ),
        (
            DurableInputKind::BackgroundExecutionReport,
            json!({
                "kind": "backgroundExecutionReport",
                "executionID": "bge_1",
                "status": "completed",
                "title": "cargo build",
                "command": "cargo build",
                "purpose": "build",
                "requiresAuthoritativeRefresh": false,
                "exitCode": 0,
                "timedOut": false,
                "error": null,
                "text": "done"
            }),
        ),
        (
            DurableInputKind::HumanRequestAnswer,
            json!({
                "kind": "humanRequestAnswer",
                "requestID": "hrq_1",
                "humanRequestKind": "input",
                "text": "answer",
                "request": {"question": "which?"},
                "response": {"answer": "this one"}
            }),
        ),
        (
            DurableInputKind::SessionMessage,
            json!({
                "kind": "sessionMessage",
                "schemaVersion": 1,
                "fromSessionID": "ses_source",
                "fromAgent": "orchestrator",
                "text": "peer context"
            }),
        ),
        (
            DurableInputKind::HostMessage,
            json!({"message": {"id": "msg_1"}, "parts": []}),
        ),
    ]
}

#[test]
fn every_published_prompt_shape_classifies_to_exactly_one_kind() {
    for (expected, prompt) in published_prompt_shapes() {
        assert_eq!(
            DurableInputKind::classify(&prompt),
            Some(expected),
            "published shape {prompt} must classify"
        );
    }
}

#[test]
fn a_prompt_shape_no_writer_publishes_is_recognized_as_undrivable() {
    for prompt in [
        json!({"kind": "somethingLater", "text": "hello"}),
        json!({"text": "hello"}),
        json!({}),
        json!({"kind": 7}),
        json!("hello"),
    ] {
        assert_eq!(
            DurableInputKind::classify(&prompt),
            None,
            "shape {prompt} has no writer, so a driver must be able to skip it"
        );
    }
}

#[test]
fn the_kind_discriminator_round_trips_for_every_shape_that_writes_one() {
    for (kind, prompt) in published_prompt_shapes() {
        assert_eq!(
            kind.as_str(),
            prompt.get("kind").and_then(serde_json::Value::as_str),
            "{kind:?} must report the discriminator its writer serializes"
        );
    }
}

#[test]
fn only_settled_reports_are_delivered_by_the_idle_wake_path() {
    let asynchronous = published_prompt_shapes()
        .into_iter()
        .filter(|(kind, _)| kind.is_asynchronous_report())
        .map(|(kind, _)| kind)
        .collect::<Vec<_>>();
    assert_eq!(
        asynchronous,
        [
            DurableInputKind::SubagentReport,
            DurableInputKind::ProductAgentReport,
            DurableInputKind::WorkflowReport,
            DurableInputKind::CouncilReport,
            DurableInputKind::BackgroundExecutionReport,
        ],
        "a live user submission must never be batched as a settled report"
    );
}

#[test]
fn a_shape_whose_payload_is_not_plain_text_refuses_to_be_read_as_text() {
    for (kind, prompt) in published_prompt_shapes() {
        let text = kind.plain_text(&prompt);
        match kind {
            DurableInputKind::TuiPrompt
            | DurableInputKind::User
            | DurableInputKind::HostMessage => assert_eq!(
                text, None,
                "{kind:?} carries structured payload only its own surface renders"
            ),
            _ => assert!(
                text.is_some(),
                "{kind:?} is delivered as text, so its text must be reachable"
            ),
        }
    }
}

#[test]
fn only_the_acp_shape_carries_content_blocks_alongside_its_text() {
    for (kind, prompt) in published_prompt_shapes() {
        let blocks = kind.content_blocks(&prompt);
        if kind == DurableInputKind::AcpPrompt {
            assert_eq!(
                blocks.map(Vec::len),
                Some(1),
                "dropping ACP content blocks would drop admitted images"
            );
        } else {
            assert_eq!(blocks, None, "{kind:?} publishes no content blocks");
        }
    }
}

fn report(id: &str, job_id: &str, text: &str, time: i64) -> NewSessionInput {
    NewSessionInput::new(
        id,
        SESSION_ID,
        json!({
            "kind": "subagentReport",
            "jobID": job_id,
            "childSessionID": "ses_child",
            "status": "completed",
            "text": text
        }),
        InputDelivery::Queue,
        time,
    )
}

#[test]
fn one_batch_promotion_claims_every_settled_report_in_fifo_order() {
    let pool = initialized(&DbLocation::Memory);
    let inbox = SessionInbox::new(Arc::clone(&pool));
    let log = SessionEventLog::new(pool);
    inbox
        .admit(report("report-1", "job_1", "first", 10))
        .expect("first report");
    inbox
        .admit(report("report-2", "job_1", "second", 11))
        .expect("second report");
    inbox
        .admit(report("report-3", "job_2", "third", 12))
        .expect("third report");

    let promoted = inbox
        .promote_pending_async(SESSION_ID)
        .expect("batch promotion");

    assert_eq!(
        promoted
            .iter()
            .map(|input| input.id.as_str())
            .collect::<Vec<_>>(),
        ["report-1", "report-2", "report-3"],
        "the batch must stay in admission order so the newest report reads as newest"
    );
    for input in &promoted {
        assert_eq!(input.state, SubmissionState::Promoted);
        assert!(input.promoted_sequence.is_some());
    }
    assert_eq!(inbox.pending(SESSION_ID).expect("pending"), []);
    let promotions = log
        .read_after(SESSION_ID, None)
        .expect("events")
        .into_iter()
        .filter(|event| event.event_type == "session.input.promoted")
        .map(|event| event.properties["inputID"].clone())
        .collect::<Vec<_>>();
    assert_eq!(
        promotions,
        ["report-1", "report-2", "report-3"],
        "every report keeps its own promotion event; the batch merges nothing"
    );
}

#[test]
fn batch_promotion_leaves_live_submissions_for_their_own_driver() {
    let pool = initialized(&DbLocation::Memory);
    let inbox = SessionInbox::new(pool);
    inbox
        .admit(input("typed", "what changed?", InputDelivery::Queue, 10))
        .expect("user submission");
    inbox
        .admit(report("report-1", "job_1", "done", 11))
        .expect("report");
    inbox
        .admit(input("steered", "stop", InputDelivery::Steer, 12))
        .expect("steer");

    let promoted = inbox
        .promote_pending_async(SESSION_ID)
        .expect("batch promotion");

    assert_eq!(
        promoted
            .iter()
            .map(|input| input.id.as_str())
            .collect::<Vec<_>>(),
        ["report-1"]
    );
    assert_eq!(
        inbox
            .pending(SESSION_ID)
            .expect("pending")
            .into_iter()
            .map(|input| input.id)
            .collect::<Vec<_>>(),
        ["typed", "steered"],
        "a batched wake must not swallow input the user is still waiting on"
    );
}

#[test]
fn a_second_batch_promotion_finds_nothing_left_to_claim() {
    let pool = initialized(&DbLocation::Memory);
    let inbox = SessionInbox::new(pool);
    inbox
        .admit(report("report-1", "job_1", "first", 10))
        .expect("first report");
    inbox
        .admit(report("report-2", "job_2", "second", 11))
        .expect("second report");

    assert_eq!(
        inbox
            .promote_pending_async(SESSION_ID)
            .expect("batch promotion")
            .len(),
        2
    );

    assert_eq!(
        inbox
            .promote_pending_async(SESSION_ID)
            .expect("repeat batch promotion"),
        [],
        "a losing wake must find an empty batch instead of replaying delivered reports"
    );
}

#[test]
fn two_concurrent_batch_promoters_split_the_reports_without_double_claiming() {
    let pool = initialized(&DbLocation::Memory);
    let inbox = SessionInbox::new(pool);
    for index in 0..6 {
        inbox
            .admit(report(
                &format!("report-{index}"),
                &format!("job_{index}"),
                "done",
                10 + index,
            ))
            .expect("report admission");
    }

    let left = {
        let inbox = inbox.clone();
        std::thread::spawn(move || {
            inbox
                .promote_pending_async(SESSION_ID)
                .expect("left batch promotion")
        })
    };
    let right = {
        let inbox = inbox.clone();
        std::thread::spawn(move || {
            inbox
                .promote_pending_async(SESSION_ID)
                .expect("right batch promotion")
        })
    };

    let mut claimed = left.join().expect("left thread");
    claimed.extend(right.join().expect("right thread"));
    let mut ids = claimed
        .into_iter()
        .map(|input| input.id)
        .collect::<Vec<_>>();
    ids.sort();
    ids.dedup();
    assert_eq!(
        ids.len(),
        6,
        "each report must be promoted exactly once across both batches"
    );
    assert_eq!(inbox.pending(SESSION_ID).expect("pending"), []);
}
