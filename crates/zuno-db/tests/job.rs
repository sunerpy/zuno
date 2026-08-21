use serde_json::json;
use std::sync::Arc;
use zuno_db::event_log::SessionEventLog;
use zuno_db::inbox::{InputDelivery, NewSessionInput, SessionInbox};
use zuno_db::job::{AgentJobStore, JobSettlement, JobStatus, NewAgentJob, ReportDelivery};
use zuno_db::{Pool, migration, session};
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
    NewAgentJob::new(id, PARENT, CHILD, delivery, 10)
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
        InputDelivery::NextStep,
        20,
    )
}

#[test]
fn creating_a_job_persists_running_state_and_parent_event() {
    let pool = initialized(&DbLocation::Memory);
    let store = AgentJobStore::new(Arc::clone(&pool));

    let created = store
        .create(running("job_1", ReportDelivery::NextStep))
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
    assert_eq!(events[0].properties["childSessionID"], CHILD);
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
    {
        let pool = initialized(&location);
        let store = AgentJobStore::new(pool);
        store
            .create(running("job_1", ReportDelivery::NextStep))
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
