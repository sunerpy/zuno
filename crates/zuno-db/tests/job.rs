use serde_json::json;
use std::sync::Arc;
use zuno_db::event_log::SessionEventLog;
use zuno_db::inbox::{InputDelivery, NewSessionInput, SessionInbox};
use zuno_db::job::{
    AgentJobStore, JobSettlement, JobStatus, JobSubject, NewAgentJob, ReportDelivery,
};
use zuno_db::{Pool, migration, session};
use zuno_orchestration::AttemptSnapshot;
use zuno_paths::DbLocation;

const PARENT: &str = "ses_parent";
const CHILD: &str = "ses_child";

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
                PARENT,
                "parent",
                "project",
                "/workspace",
                "/workspace",
                "Parent",
                "zuno",
            )
            .at(1),
        )?;
        session::create(
            transaction,
            &session::SessionCreate::new(
                CHILD,
                "child",
                "project",
                "/workspace",
                "/workspace",
                "Child",
                "zuno",
            )
            .with_parent(PARENT)
            .at(2),
        )?;
        Ok(())
    })
    .expect("create sessions");
    pool
}

fn running(id: &str, delivery: ReportDelivery) -> NewAgentJob {
    NewAgentJob::new(id, PARENT, JobSubject::child_session(CHILD), delivery, 10)
}

fn report(id: &str) -> NewSessionInput {
    NewSessionInput::new(
        id,
        PARENT,
        json!({
            "kind": "subagentReport",
            "jobID": "job_1",
            "childSessionID": CHILD,
            "text": "child result"
        }),
        InputDelivery::Queue,
        20,
    )
}

fn orchestration_snapshot() -> AttemptSnapshot {
    serde_json::from_value(json!({
        "schemaVersion": 2,
        "turnId": "turn-parent",
        "step": 1,
        "capability": {
            "schemaVersion": 2,
            "pack": {"id":"test","version":"1","upstreamRevision":"test"},
            "extensionRevision": 0,
            "permissionPolicySha256": "policy",
            "profiles": [], "presets": [], "councils": [], "workflows": [], "skills": []
        },
        "owner": {
            "sessionId":PARENT, "parentSessionId":null, "parentAttempt":null,
            "workflow":null, "workflowNode":null
        },
        "agent": {
            "name":"build", "sourceId":"test://build",
            "definitionSha256":"definition", "permissionSha256":"permission",
            "promptPolicySha256":"prompt"
        },
        "model": {
            "providerId":"fake", "modelId":"fake-model", "wireModelId":"fake-model",
            "surface":"responses", "reasoningSha256":"reasoning", "preset":null
        },
        "selectedSkills": [],
        "prompt": {"eventId":"evt-parent","assemblySha256":"assembly","actualSha256":"actual"},
        "tools": []
    }))
    .expect("test orchestration snapshot")
}

#[test]
fn creating_a_job_persists_running_state_and_parent_event() {
    let pool = initialized(&DbLocation::Memory);
    let store = AgentJobStore::new(Arc::clone(&pool));
    let snapshot = orchestration_snapshot();

    let created = store
        .create(
            running("job_1", ReportDelivery::NextStep)
                .with_orchestration_snapshot(Some(snapshot.clone())),
        )
        .expect("create job");

    assert_eq!(created.status, JobStatus::Running);
    assert_eq!(created.created_sequence, 0);
    assert_eq!(store.get("job_1").expect("stored job"), created);
    let events = SessionEventLog::new(pool)
        .read_after(PARENT, None)
        .expect("parent events");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event_type, "agent.job.created");
    assert_eq!(events[0].properties["jobID"], "job_1");
    assert_eq!(created.orchestration_snapshot.as_ref(), Some(&snapshot));
    assert_eq!(
        events[0].properties["orchestrationSnapshot"],
        snapshot.canonical_value().expect("canonical snapshot")
    );
    assert_eq!(
        events[0].properties["orchestrationSnapshotID"],
        serde_json::to_value(snapshot.identity().expect("snapshot identity"))
            .expect("snapshot identity JSON")
    );
    assert_eq!(
        events[0].properties["subject"],
        json!({"kind":"childSession","sessionID":CHILD})
    );
}

#[test]
fn queued_job_becomes_running_only_after_admission() {
    let pool = initialized(&DbLocation::Memory);
    let store = AgentJobStore::new(Arc::clone(&pool));

    let created = store
        .create(running("job_queued", ReportDelivery::Quiet).queued())
        .expect("create queued job");
    assert_eq!(created.status, JobStatus::Queued);
    assert_eq!(created.time_updated, 10);

    let started = store.start("job_queued", 15).expect("start queued job");
    assert_eq!(started.status, JobStatus::Running);
    assert_eq!(started.time_updated, 15);

    let events = SessionEventLog::new(pool)
        .read_after(PARENT, None)
        .expect("parent events");
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].event_type, "agent.job.created");
    assert_eq!(events[0].properties["status"], "queued");
    assert_eq!(events[1].event_type, "agent.job.started");
    assert_eq!(events[1].properties["jobID"], "job_queued");
}

#[test]
fn product_agent_jobs_have_no_fake_child_session_and_can_be_uncertain() {
    let pool = initialized(&DbLocation::Memory);
    let store = AgentJobStore::new(Arc::clone(&pool));
    let snapshot = orchestration_snapshot();
    let created = store
        .create(
            NewAgentJob::new(
                "job_product",
                PARENT,
                JobSubject::product_agent("run_product", "codex", "codex-work", "subagent_codex"),
                ReportDelivery::Quiet,
                10,
            )
            .with_orchestration_snapshot(Some(snapshot.clone())),
        )
        .expect("create product job");

    assert!(matches!(
        created.subject,
        JobSubject::ProductAgent { ref run_id, .. } if run_id == "run_product"
    ));
    let settled = store
        .settle(
            "job_product",
            JobSettlement::uncertain("app-server disconnected", 20, None),
        )
        .expect("mark uncertain");
    assert_eq!(settled.job.status, JobStatus::Uncertain);
    assert_eq!(settled.job.orchestration_snapshot.as_ref(), Some(&snapshot));
    assert_eq!(
        store
            .running_product_agents_for(PARENT)
            .expect("running products"),
        Vec::new()
    );
}

#[test]
fn workflow_jobs_are_first_class_durable_subjects() {
    let pool = initialized(&DbLocation::Memory);
    let store = AgentJobStore::new(Arc::clone(&pool));
    let created = store
        .create(NewAgentJob::new(
            "job_workflow",
            PARENT,
            JobSubject::workflow("run_release", "release-hardening"),
            ReportDelivery::Quiet,
            10,
        ))
        .expect("create workflow job");
    assert_eq!(
        created.subject.as_json(),
        json!({"kind":"workflow","runID":"run_release","workflow":"release-hardening"})
    );
    assert_eq!(
        store
            .running_workflows_for(PARENT)
            .expect("running workflows")
            .into_iter()
            .map(|job| job.id)
            .collect::<Vec<_>>(),
        ["job_workflow"]
    );
}

#[test]
fn durable_product_subjects_reject_blank_identifiers() {
    let pool = initialized(&DbLocation::Memory);
    let connection = pool.get().expect("database connection");
    let error = connection
        .execute(
            r#"INSERT INTO agent_job (
               id, parent_session_id, subject_kind, subject_payload, status, report_delivery,
               created_seq, time_created, time_updated
             ) VALUES (
               'job_blank_product', ?1, 'product-agent',
               '{"kind":"productAgent","runID":"   ","product":"codex","instance":"reviewer","tool":"subagent_codex"}',
               'running', 'quiet', 0, 1, 1
             )"#,
            [PARENT],
        )
        .expect_err("blank durable product identifiers must fail");
    assert!(error.to_string().contains("agent_job_subject"), "{error}");
}

#[test]
fn next_step_settlement_updates_the_job_and_admits_the_report_atomically() {
    let pool = initialized(&DbLocation::Memory);
    let store = AgentJobStore::new(Arc::clone(&pool));
    store
        .create(running("job_1", ReportDelivery::NextStep))
        .expect("create job");

    let settled = store
        .settle(
            "job_1",
            JobSettlement::completed(json!({"text": "child result"}), 20, Some(report("input_1"))),
        )
        .expect("settle job");

    assert_eq!(settled.job.status, JobStatus::Completed);
    assert_eq!(settled.job.report_input_id.as_deref(), Some("input_1"));
    assert_eq!(
        settled.report.as_ref().map(|input| input.id.as_str()),
        Some("input_1")
    );
    assert_eq!(
        SessionInbox::new(Arc::clone(&pool))
            .pending(PARENT)
            .expect("pending reports"),
        [settled.report.expect("report")]
    );
    let events = SessionEventLog::new(pool)
        .read_after(PARENT, None)
        .expect("parent events");
    assert_eq!(
        events
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        [
            "agent.job.created",
            "agent.job.settled",
            "session.input.admitted"
        ]
    );
}

#[test]
fn quiet_settlement_records_the_result_without_parent_input() {
    let pool = initialized(&DbLocation::Memory);
    let store = AgentJobStore::new(Arc::clone(&pool));
    store
        .create(running("job_1", ReportDelivery::Quiet))
        .expect("create job");

    let settled = store
        .settle(
            "job_1",
            JobSettlement::completed(json!({"text": "quiet result"}), 20, None),
        )
        .expect("settle quiet job");

    assert_eq!(settled.job.status, JobStatus::Completed);
    assert_eq!(settled.report, None);
    assert!(
        SessionInbox::new(pool)
            .pending(PARENT)
            .expect("pending")
            .is_empty()
    );
}

#[test]
fn failed_cancelled_and_uncertain_settlements_can_retain_structured_results() {
    let pool = initialized(&DbLocation::Memory);
    let store = AgentJobStore::new(Arc::clone(&pool));

    for (job_id, status, message) in [
        (
            "job_failed",
            JobStatus::Failed,
            "provider rejected the request",
        ),
        (
            "job_cancelled",
            JobStatus::Cancelled,
            "cancelled by the user",
        ),
        (
            "job_uncertain",
            JobStatus::Uncertain,
            "executor acknowledgement was lost",
        ),
    ] {
        store
            .create(running(job_id, ReportDelivery::Quiet))
            .expect("create job");
        let expected = json!({
            "job": job_id,
            "partial": true,
            "items": ["durable", "structured"]
        });
        let settlement = match status {
            JobStatus::Failed => JobSettlement::failed(message, 20, None),
            JobStatus::Cancelled => JobSettlement::cancelled(message, 20, None),
            JobStatus::Uncertain => JobSettlement::uncertain(message, 20, None),
            JobStatus::Queued | JobStatus::Running | JobStatus::Completed => {
                unreachable!("test terminal status")
            }
        }
        .with_result(expected.clone());

        let settled = store
            .settle(job_id, settlement)
            .expect("settle job with structured result");

        assert_eq!(settled.job.status, status);
        assert_eq!(settled.job.result.as_ref(), Some(&expected));
        assert_eq!(settled.job.error.as_deref(), Some(message));
        let event = SessionEventLog::new(Arc::clone(&pool))
            .read_after(PARENT, None)
            .expect("parent events")
            .into_iter()
            .find(|event| {
                event.event_type == "agent.job.settled"
                    && event
                        .properties
                        .get("jobID")
                        .and_then(serde_json::Value::as_str)
                        == Some(job_id)
            })
            .expect("settled event");
        assert_eq!(event.properties.get("result"), Some(&expected));
        assert_eq!(event.properties.get("error"), Some(&json!(message)));
    }
}

#[test]
fn structured_results_do_not_weaken_terminal_settlement_invariants() {
    let pool = initialized(&DbLocation::Memory);
    let store = AgentJobStore::new(Arc::clone(&pool));
    store
        .create(running("job_1", ReportDelivery::Quiet))
        .expect("create job");

    let completed_error = store
        .settle(
            "job_1",
            JobSettlement {
                status: JobStatus::Completed,
                result: Some(json!({"text": "complete"})),
                error: Some("completed jobs must not carry errors".to_owned()),
                time_completed: 20,
                report: None,
            },
        )
        .expect_err("completed settlement with an error must be rejected");
    assert_eq!(
        std::error::Error::source(&completed_error)
            .expect("completed validation source")
            .to_string(),
        "a completed job requires a result and no error"
    );

    let failed_without_error = store
        .settle(
            "job_1",
            JobSettlement {
                status: JobStatus::Failed,
                result: Some(json!({"partial": true})),
                error: Some(String::new()),
                time_completed: 20,
                report: None,
            },
        )
        .expect_err("failed settlement without an error must be rejected");
    assert_eq!(
        std::error::Error::source(&failed_without_error)
            .expect("failed validation source")
            .to_string(),
        "a failed, cancelled, or uncertain job requires an error"
    );
    assert_eq!(
        store.get("job_1").expect("running job").status,
        JobStatus::Running
    );
}

#[test]
fn failed_report_admission_rolls_back_job_state_and_event_sequence() {
    let pool = initialized(&DbLocation::Memory);
    let store = AgentJobStore::new(Arc::clone(&pool));
    store
        .create(running("job_1", ReportDelivery::NextStep))
        .expect("create job");
    SessionInbox::new(Arc::clone(&pool))
        .admit(report("duplicate"))
        .expect("occupy report id");

    store
        .settle(
            "job_1",
            JobSettlement::completed(
                json!({"text": "child result"}),
                20,
                Some(report("duplicate")),
            ),
        )
        .expect_err("duplicate report id must roll the settlement back");

    assert_eq!(store.get("job_1").expect("job").status, JobStatus::Running);
    let events = SessionEventLog::new(pool)
        .read_after(PARENT, None)
        .expect("parent events");
    assert_eq!(
        events
            .iter()
            .map(|event| (event.sequence, event.event_type.as_str()))
            .collect::<Vec<_>>(),
        [(0, "agent.job.created"), (1, "session.input.admitted")]
    );
}

#[test]
fn a_second_settlement_cannot_duplicate_the_parent_report() {
    let pool = initialized(&DbLocation::Memory);
    let store = AgentJobStore::new(Arc::clone(&pool));
    store
        .create(running("job_1", ReportDelivery::NextStep))
        .expect("create job");
    store
        .settle(
            "job_1",
            JobSettlement::completed(json!({"text": "first"}), 20, Some(report("input_1"))),
        )
        .expect("first settlement");

    store
        .settle(
            "job_1",
            JobSettlement::failed("late failure", 30, Some(report("input_2"))),
        )
        .expect_err("a terminal job cannot settle again");

    assert_eq!(
        SessionInbox::new(pool)
            .pending(PARENT)
            .expect("pending")
            .into_iter()
            .map(|input| input.id)
            .collect::<Vec<_>>(),
        ["input_1"]
    );
}

#[test]
fn pending_reports_survive_pool_reconstruction_until_promoted() {
    let directory = tempfile::tempdir().expect("temporary database directory");
    let location = DbLocation::File(directory.path().join("zuno.db"));
    let snapshot = orchestration_snapshot();
    {
        let pool = initialized(&location);
        let store = AgentJobStore::new(pool);
        store
            .create(
                running("job_1", ReportDelivery::NextStep)
                    .with_orchestration_snapshot(Some(snapshot.clone())),
            )
            .expect("create job");
        store
            .settle(
                "job_1",
                JobSettlement::completed(json!({"text": "recover"}), 20, Some(report("input_1"))),
            )
            .expect("settle job");
    }

    let pool = initialized(&location);
    let store = AgentJobStore::new(Arc::clone(&pool));
    assert_eq!(
        store
            .get("job_1")
            .expect("reopened job")
            .orchestration_snapshot
            .as_ref(),
        Some(&snapshot)
    );
    assert_eq!(
        store
            .pending_reports()
            .expect("pending report jobs")
            .into_iter()
            .map(|job| job.id)
            .collect::<Vec<_>>(),
        ["job_1"]
    );
    SessionInbox::new(Arc::clone(&pool))
        .promote_id(PARENT, "input_1")
        .expect("promote report")
        .expect("pending report");
    assert!(
        store
            .pending_reports()
            .expect("pending report jobs")
            .is_empty()
    );
}

#[test]
fn pending_reports_can_be_recovered_for_one_parent_without_waking_another() {
    let pool = initialized(&DbLocation::Memory);
    let store = AgentJobStore::new(Arc::clone(&pool));
    store
        .create(running("job_1", ReportDelivery::NextStep))
        .expect("create job");
    store
        .settle(
            "job_1",
            JobSettlement::completed(json!({"text": "child result"}), 20, Some(report("input_1"))),
        )
        .expect("settle job");

    assert_eq!(
        store
            .pending_reports_for(PARENT)
            .expect("parent pending reports")
            .into_iter()
            .map(|job| job.id)
            .collect::<Vec<_>>(),
        ["job_1"]
    );
    assert!(
        store
            .pending_reports_for("ses_other")
            .expect("other parent pending reports")
            .is_empty()
    );
}
