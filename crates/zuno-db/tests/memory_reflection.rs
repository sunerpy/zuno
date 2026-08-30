use std::sync::Arc;

use zuno_db::memory_reflection::{
    MemoryReflectionStore, ReflectionAdmission, ReflectionAdmissionResult, ReflectionJobStatus,
    ReflectionTrigger,
};
use zuno_db::{Pool, migration, session};
use zuno_paths::DbLocation;

const SESSION_ID: &str = "ses_memory_reflection";

fn initialized() -> Arc<Pool> {
    let pool = Arc::new(Pool::open(&DbLocation::Memory).expect("open database"));
    {
        let mut connection = pool.get().expect("database connection");
        migration::apply(&mut connection).expect("apply schema");
        connection
            .execute(
                "INSERT INTO project
                 (id, worktree, time_created, time_updated, sandboxes)
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
                "reflection",
                "project",
                "/workspace",
                "/workspace",
                "Reflection",
                "zuno",
            )
            .at(1),
        )
        .map(|_| ())
    })
    .expect("create session");
    pool
}

fn admission(
    message: &str,
    job: &str,
    interval: u64,
    recovered: bool,
    negative_learning: bool,
    now: i64,
) -> ReflectionAdmission {
    ReflectionAdmission {
        job_id: job.to_owned(),
        session_id: SESSION_ID.to_owned(),
        source_message_id: message.to_owned(),
        turn_interval: interval,
        recovered,
        negative_learning,
        owner_id: "runtime-1".to_owned(),
        lease_expires: now + 60_000,
        time_created: now,
    }
}

#[test]
fn delivered_turn_counter_survives_store_reopen_and_schedules_the_tenth() {
    let pool = initialized();
    for turn in 1..10 {
        let store = MemoryReflectionStore::new(Arc::clone(&pool));
        let result = store
            .admit_and_start(admission(
                &format!("msg_{turn}"),
                &format!("job_{turn}"),
                10,
                false,
                false,
                turn,
            ))
            .expect("admit delivered turn");
        assert_eq!(
            result,
            ReflectionAdmissionResult::NotScheduled {
                ordinal: turn as u64
            }
        );
    }

    let reopened = MemoryReflectionStore::new(Arc::clone(&pool));
    let result = reopened
        .admit_and_start(admission("msg_10", "job_10", 10, false, false, 10))
        .expect("admit tenth turn");
    let ReflectionAdmissionResult::Started { ordinal, job } = result else {
        panic!("tenth delivered turn did not start reflection");
    };
    assert_eq!(ordinal, 10);
    assert_eq!(job.trigger, ReflectionTrigger::Periodic);
    assert_eq!(reopened.delivery_count(SESSION_ID).expect("count"), 10);
}

#[test]
fn same_source_message_is_counted_and_started_at_most_once() {
    let store = MemoryReflectionStore::new(initialized());
    let first = store
        .admit_and_start(admission("msg_1", "job_1", 1, false, false, 10))
        .expect("first admission");
    assert!(matches!(
        first,
        ReflectionAdmissionResult::Started { ordinal: 1, .. }
    ));

    let duplicate = store
        .admit_and_start(admission("msg_1", "different-job-id", 1, false, false, 11))
        .expect("duplicate admission");
    let ReflectionAdmissionResult::AlreadyRecorded { ordinal, job } = duplicate else {
        panic!("duplicate source message was not detected");
    };
    assert_eq!(ordinal, 1);
    assert_eq!(job.expect("existing job").id, "job_1");
    assert_eq!(store.delivery_count(SESSION_ID).expect("count"), 1);
}

#[test]
fn recovery_triggers_early_but_negative_learning_only_advances_the_cadence() {
    let store = MemoryReflectionStore::new(initialized());
    let recovery = store
        .admit_and_start(admission(
            "msg_recovery",
            "job_recovery",
            10,
            true,
            false,
            10,
        ))
        .expect("recovery admission");
    let ReflectionAdmissionResult::Started { job, .. } = recovery else {
        panic!("verified recovery did not start reflection");
    };
    assert_eq!(job.trigger, ReflectionTrigger::Recovery);

    let negative = store
        .admit_and_start(admission("msg_negative", "job_negative", 2, true, true, 20))
        .expect("negative-learning admission");
    assert_eq!(
        negative,
        ReflectionAdmissionResult::NotScheduled { ordinal: 2 }
    );
    assert_eq!(store.list_for_session(SESSION_ID).expect("jobs").len(), 1);
}

#[test]
fn expired_running_job_becomes_uncertain_and_is_never_replayed() {
    let store = MemoryReflectionStore::new(initialized());
    store
        .admit_and_start(admission("msg_1", "job_1", 1, false, false, 10))
        .expect("start job");

    assert_eq!(store.reconcile_expired(60_010).expect("reconcile"), 1);
    let job = store.get("job_1").expect("job");
    assert_eq!(job.status, ReflectionJobStatus::Uncertain);
    assert!(
        job.error
            .as_deref()
            .is_some_and(|error| { error.contains("without an authoritative outcome") })
    );

    let duplicate = store
        .admit_and_start(admission("msg_1", "job_2", 1, false, false, 70_000))
        .expect("duplicate admission");
    let ReflectionAdmissionResult::AlreadyRecorded { job: Some(job), .. } = duplicate else {
        panic!("uncertain job was replayed");
    };
    assert_eq!(job.status, ReflectionJobStatus::Uncertain);
}

#[test]
fn owner_can_settle_a_running_job_once() {
    let store = MemoryReflectionStore::new(initialized());
    store
        .admit_and_start(admission("msg_1", "job_1", 1, false, false, 10))
        .expect("start job");
    let settled = store
        .settle(
            "job_1",
            "runtime-1",
            ReflectionJobStatus::Completed,
            None,
            20,
        )
        .expect("settle job");
    assert_eq!(settled.status, ReflectionJobStatus::Completed);
    assert!(
        store
            .settle(
                "job_1",
                "runtime-1",
                ReflectionJobStatus::Failed,
                Some("late failure"),
                30,
            )
            .is_err()
    );
}
