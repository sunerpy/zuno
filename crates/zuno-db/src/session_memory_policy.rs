//! Revisioned per-session memory and learning-generation policy.

use crate::event_log::{NewSessionEvent, append_in, query_error};
use crate::{Pool, open};
use rusqlite::{OptionalExtension as _, Row, Transaction, params};
use serde_json::{Map, Value, json};
use std::sync::Arc;
use zuno_error::DbError;
use zuno_types::{SessionMemoryGeneration, SessionMemoryPolicyProjection};

const COLUMNS: &str =
    "session_id, use_memories, generation, reason, source, revision, time_created, time_updated";

/// One compare-and-set policy update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionMemoryPolicyUpdate {
    pub session_id: String,
    pub use_memories: bool,
    pub generation: SessionMemoryGeneration,
    pub reason: String,
    pub source: String,
    /// Zero creates the first durable row; later writes name the current revision.
    pub expected_revision: i64,
    pub time_updated: i64,
}

/// Exclude one session from generation and retire its queued extraction work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionMemoryPolicyExclusion {
    pub session_id: String,
    pub use_memories: bool,
    pub reason: String,
    pub source: String,
    /// Zero creates the first durable row; later writes name the current revision.
    pub expected_revision: i64,
    pub time_updated: i64,
}

/// One committed policy mutation and its queue effect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionMemoryPolicyApplied {
    pub policy: SessionMemoryPolicyProjection,
    pub skipped_auto_extraction_jobs: usize,
}

/// A compare-and-set mutation either commits or reports the current durable row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionMemoryPolicyWrite {
    Applied(SessionMemoryPolicyApplied),
    /// `None` means no durable row exists and the current revision is zero.
    Stale(Option<SessionMemoryPolicyProjection>),
}

struct StoredPolicy {
    session_id: String,
    use_memories: i64,
    generation: String,
    reason: String,
    source: String,
    revision: i64,
    time_created: i64,
    time_updated: i64,
}

/// Typed access to one optional policy row per durable session.
#[derive(Clone)]
pub struct SessionMemoryPolicyStore {
    pool: Arc<Pool>,
}

/// Freeze one new session's caller default in the transaction that creates it.
///
/// An existing row wins so a retried materialization cannot overwrite a policy
/// another writer already committed.
pub fn seed_in(
    transaction: &Transaction<'_>,
    session_id: &str,
    use_memories: bool,
    generation: SessionMemoryGeneration,
    reason: &str,
    source: &str,
    now: i64,
) -> Result<SessionMemoryPolicyProjection, DbError> {
    validate_text("session id", session_id)?;
    validate_text("policy reason", reason)?;
    validate_text("policy source", source)?;
    validate_time(now)?;
    ensure_session(transaction, session_id)?;
    if let Some(current) = read_optional(transaction, session_id)? {
        return Ok(current);
    }
    transaction
        .execute(
            "INSERT INTO session_memory_policy (
               session_id, use_memories, generation, reason, source, revision,
               time_created, time_updated
             ) VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6, ?6)",
            params![
                session_id,
                i64::from(use_memories),
                generation.as_str(),
                reason,
                source,
                now,
            ],
        )
        .map_err(open::map_error)?;
    let stored = read_required(transaction, session_id)?;
    append_policy_event(
        transaction,
        session_id,
        PolicyOperation::Seed,
        &stored,
        0,
        0,
    )?;
    Ok(stored)
}

/// Copy the parent's effective durable policy into a newly created child session.
///
/// A released parent with no policy row keeps the pre-policy behavior: memory use
/// and generation are enabled. The child receives its own revisioned row so later
/// parent changes cannot silently widen or narrow delegated work.
pub fn inherit_in(
    transaction: &Transaction<'_>,
    parent_session_id: &str,
    child_session_id: &str,
    parent_default: SessionMemoryPolicyProjection,
    now: i64,
) -> Result<SessionMemoryPolicyProjection, DbError> {
    ensure_session(transaction, parent_session_id)?;
    if parent_default.revision != 0 {
        return Err(query_error(std::io::Error::other(
            "a parent session memory-policy default must have revision zero",
        )));
    }
    let parent = read_optional(transaction, parent_session_id)?.unwrap_or(parent_default);
    seed_in(
        transaction,
        child_session_id,
        parent.use_memories,
        parent.generation,
        &format!("inherited from parent session {parent_session_id}"),
        "parent_session",
        now,
    )
}

impl SessionMemoryPolicyStore {
    #[must_use]
    pub fn new(pool: Arc<Pool>) -> Self {
        Self { pool }
    }

    /// Freeze one existing session's caller default unless another writer already did.
    pub fn seed(
        &self,
        session_id: &str,
        use_memories: bool,
        generation: SessionMemoryGeneration,
        reason: &str,
        source: &str,
        now: i64,
    ) -> Result<SessionMemoryPolicyProjection, DbError> {
        self.pool.transaction(|transaction| {
            seed_in(
                transaction,
                session_id,
                use_memories,
                generation,
                reason,
                source,
                now,
            )
        })
    }

    /// Read the durable row, returning `None` when this session uses its caller's default.
    pub fn get(&self, session_id: &str) -> Result<Option<SessionMemoryPolicyProjection>, DbError> {
        let connection = self.pool.get()?;
        ensure_session(&connection, session_id)?;
        read_optional(&connection, session_id)
    }

    /// Read the durable row or use the exact default supplied by the caller.
    pub fn get_or(
        &self,
        session_id: &str,
        fallback: SessionMemoryPolicyProjection,
    ) -> Result<SessionMemoryPolicyProjection, DbError> {
        if fallback.revision != 0 {
            return Err(query_error(std::io::Error::other(
                "a caller-default session memory policy must have revision zero",
            )));
        }
        Ok(self.get(session_id)?.unwrap_or(fallback))
    }

    /// Set an enabled or disabled policy and append its audit event atomically.
    pub fn set(
        &self,
        update: SessionMemoryPolicyUpdate,
    ) -> Result<SessionMemoryPolicyWrite, DbError> {
        if update.generation == SessionMemoryGeneration::Excluded {
            return Err(query_error(std::io::Error::other(
                "use SessionMemoryPolicyStore::exclude for generation=excluded",
            )));
        }
        let skip_queued = update.generation == SessionMemoryGeneration::Disabled;
        self.write(update, PolicyOperation::Set, skip_queued)
    }

    /// Set `generation=excluded`, skip queued extraction jobs, and audit both atomically.
    pub fn exclude(
        &self,
        exclusion: SessionMemoryPolicyExclusion,
    ) -> Result<SessionMemoryPolicyWrite, DbError> {
        self.write(
            SessionMemoryPolicyUpdate {
                session_id: exclusion.session_id,
                use_memories: exclusion.use_memories,
                generation: SessionMemoryGeneration::Excluded,
                reason: exclusion.reason,
                source: exclusion.source,
                expected_revision: exclusion.expected_revision,
                time_updated: exclusion.time_updated,
            },
            PolicyOperation::Exclude,
            true,
        )
    }

    /// Atomically settle every queued automatic extraction for this session as skipped.
    ///
    /// A concurrent manual reflection may briefly be queued between admission and
    /// claim, so this boundary identifies automatic work by its durable trigger.
    pub fn skip_queued_auto_extraction_jobs(
        &self,
        session_id: &str,
        reason: &str,
        source: &str,
        now: i64,
    ) -> Result<usize, DbError> {
        validate_text("session id", session_id)?;
        validate_text("skip reason", reason)?;
        validate_text("skip source", source)?;
        validate_time(now)?;
        self.pool.transaction(|transaction| {
            ensure_session(transaction, session_id)?;
            skip_queued_auto_extraction_jobs_in(
                transaction,
                session_id,
                SessionMemoryGeneration::Disabled.as_str(),
                reason,
                source,
                now,
            )
        })
    }

    fn write(
        &self,
        update: SessionMemoryPolicyUpdate,
        operation: PolicyOperation,
        skip_queued: bool,
    ) -> Result<SessionMemoryPolicyWrite, DbError> {
        validate_update(&update)?;
        self.pool.transaction(|transaction| {
            ensure_session(transaction, &update.session_id)?;
            let current = read_optional(transaction, &update.session_id)?;
            let current_revision = current.as_ref().map_or(0, |policy| policy.revision);
            if current_revision != update.expected_revision {
                return Ok(SessionMemoryPolicyWrite::Stale(current));
            }
            if current.as_ref().is_some_and(|policy| {
                policy.generation == SessionMemoryGeneration::Excluded
                    && update.generation != SessionMemoryGeneration::Excluded
            }) {
                return Err(DbError::Conflict {
                    table: "session_memory_policy".to_owned(),
                    id: update.session_id.clone(),
                    detail: "an excluded session memory policy cannot be re-enabled or downgraded"
                        .to_owned(),
                });
            }
            let revision = current_revision.checked_add(1).ok_or_else(|| {
                query_error(std::io::Error::other(
                    "session memory policy revision exhausted",
                ))
            })?;
            transaction
                .execute(
                    "INSERT INTO session_memory_policy (
                       session_id, use_memories, generation, reason, source, revision,
                       time_created, time_updated
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)
                     ON CONFLICT(session_id) DO UPDATE SET
                       use_memories = excluded.use_memories,
                       generation = excluded.generation,
                       reason = excluded.reason,
                       source = excluded.source,
                       revision = excluded.revision,
                       time_updated = excluded.time_updated",
                    params![
                        update.session_id,
                        i64::from(update.use_memories),
                        update.generation.as_str(),
                        update.reason,
                        update.source,
                        revision,
                        update.time_updated,
                    ],
                )
                .map_err(open::map_error)?;
            let stored = read_required(transaction, &update.session_id)?;
            let skipped_auto_extraction_jobs = if skip_queued {
                skip_queued_auto_extraction_jobs_in(
                    transaction,
                    &update.session_id,
                    stored.generation.as_str(),
                    stored
                        .reason
                        .as_deref()
                        .expect("durable policy rows always have a reason"),
                    stored
                        .source
                        .as_deref()
                        .expect("durable policy rows always have a source"),
                    update.time_updated,
                )?
            } else {
                0
            };
            append_policy_event(
                transaction,
                &update.session_id,
                operation,
                &stored,
                current_revision,
                skipped_auto_extraction_jobs,
            )?;
            Ok(SessionMemoryPolicyWrite::Applied(
                SessionMemoryPolicyApplied {
                    policy: stored,
                    skipped_auto_extraction_jobs,
                },
            ))
        })
    }
}

#[derive(Debug, Clone, Copy)]
enum PolicyOperation {
    Seed,
    Set,
    Exclude,
}

impl PolicyOperation {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Seed => "seed",
            Self::Set => "set",
            Self::Exclude => "exclude",
        }
    }
}

fn append_policy_event(
    transaction: &Transaction<'_>,
    session_id: &str,
    operation: PolicyOperation,
    policy: &SessionMemoryPolicyProjection,
    previous_revision: i64,
    skipped_auto_extraction_jobs: usize,
) -> Result<(), DbError> {
    let mut properties = Map::new();
    properties.insert(
        "operation".to_owned(),
        Value::String(operation.as_str().to_owned()),
    );
    properties.insert("useMemories".to_owned(), Value::Bool(policy.use_memories));
    properties.insert(
        "generation".to_owned(),
        Value::String(policy.generation.as_str().to_owned()),
    );
    properties.insert(
        "reason".to_owned(),
        policy.reason.clone().map_or(Value::Null, Value::String),
    );
    properties.insert(
        "source".to_owned(),
        policy.source.clone().map_or(Value::Null, Value::String),
    );
    properties.insert(
        "time".to_owned(),
        policy.time.map_or(Value::Null, Value::from),
    );
    properties.insert("revision".to_owned(), Value::from(policy.revision));
    properties.insert(
        "previousRevision".to_owned(),
        Value::from(previous_revision),
    );
    properties.insert(
        "skippedAutoExtractionJobs".to_owned(),
        Value::from(u64::try_from(skipped_auto_extraction_jobs).unwrap_or(u64::MAX)),
    );
    append_in(
        transaction,
        session_id,
        NewSessionEvent::new("session.memory.policy.changed", properties)?,
    )?;
    Ok(())
}

fn skip_queued_auto_extraction_jobs_in(
    transaction: &Transaction<'_>,
    session_id: &str,
    generation: &str,
    reason: &str,
    source: &str,
    now: i64,
) -> Result<usize, DbError> {
    let result = serde_json::to_string(&json!({
        "kind": "sessionMemoryPolicy",
        "generation": generation,
        "reason": reason,
        "source": source,
    }))
    .map_err(query_error)?;
    transaction
        .execute(
            "UPDATE learning_job
             SET status = 'skipped', result = ?2, error = NULL, owner_id = NULL,
                 lease_expires = NULL, time_updated = ?3, time_completed = ?3
             WHERE session_id = ?1 AND kind = 'extraction' AND status = 'queued'
               AND (
                 ?4 = 'excluded'
                 OR COALESCE(
                   json_extract(payload, '$.trigger'),
                   'automatic_post_turn'
                 ) = 'automatic_post_turn'
               )",
            params![session_id, result, now, generation],
        )
        .map_err(open::map_error)
}

fn ensure_session(connection: &rusqlite::Connection, session_id: &str) -> Result<(), DbError> {
    validate_text("session id", session_id)?;
    let exists = connection
        .query_row("SELECT 1 FROM session WHERE id = ?1", [session_id], |_| {
            Ok(())
        })
        .optional()
        .map_err(open::map_error)?
        .is_some();
    if exists {
        Ok(())
    } else {
        Err(DbError::NotFound {
            table: "session".to_owned(),
            id: session_id.to_owned(),
        })
    }
}

fn validate_update(update: &SessionMemoryPolicyUpdate) -> Result<(), DbError> {
    validate_text("session id", &update.session_id)?;
    validate_text("policy reason", &update.reason)?;
    validate_text("policy source", &update.source)?;
    if update.expected_revision < 0 {
        return Err(query_error(std::io::Error::other(
            "session memory policy expected_revision must not be negative",
        )));
    }
    validate_time(update.time_updated)
}

fn validate_text(label: &str, value: &str) -> Result<(), DbError> {
    if value.trim().is_empty() {
        return Err(query_error(std::io::Error::other(format!(
            "{label} must not be empty"
        ))));
    }
    Ok(())
}

fn validate_time(time: i64) -> Result<(), DbError> {
    if time < 0 {
        return Err(query_error(std::io::Error::other(
            "session memory policy time must not be negative",
        )));
    }
    Ok(())
}

fn read_required(
    connection: &rusqlite::Connection,
    session_id: &str,
) -> Result<SessionMemoryPolicyProjection, DbError> {
    read_optional(connection, session_id)?.ok_or_else(|| DbError::NotFound {
        table: "session_memory_policy".to_owned(),
        id: session_id.to_owned(),
    })
}

fn read_optional(
    connection: &rusqlite::Connection,
    session_id: &str,
) -> Result<Option<SessionMemoryPolicyProjection>, DbError> {
    connection
        .query_row(
            &format!("SELECT {COLUMNS} FROM session_memory_policy WHERE session_id = ?1"),
            [session_id],
            decode_row,
        )
        .optional()
        .map_err(open::map_error)?
        .map(decode)
        .transpose()
}

fn decode_row(row: &Row<'_>) -> rusqlite::Result<StoredPolicy> {
    Ok(StoredPolicy {
        session_id: row.get(0)?,
        use_memories: row.get(1)?,
        generation: row.get(2)?,
        reason: row.get(3)?,
        source: row.get(4)?,
        revision: row.get(5)?,
        time_created: row.get(6)?,
        time_updated: row.get(7)?,
    })
}

fn decode(row: StoredPolicy) -> Result<SessionMemoryPolicyProjection, DbError> {
    let use_memories = match row.use_memories {
        0 => false,
        1 => true,
        value => {
            return Err(query_error(std::io::Error::other(format!(
                "unknown use_memories value `{value}` for session `{}`",
                row.session_id
            ))));
        }
    };
    let generation = SessionMemoryGeneration::parse(&row.generation).ok_or_else(|| {
        query_error(std::io::Error::other(format!(
            "unknown session memory generation `{}`",
            row.generation
        )))
    })?;
    if row.revision < 1 || row.time_created < 0 || row.time_updated < row.time_created {
        return Err(query_error(std::io::Error::other(format!(
            "invalid session memory policy audit fields for `{}`",
            row.session_id
        ))));
    }
    Ok(SessionMemoryPolicyProjection {
        use_memories,
        generation,
        reason: Some(row.reason),
        source: Some(row.source),
        time: Some(row.time_updated),
        revision: row.revision,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::learning_job::{LearningJobStatus, LearningJobStore, NewLearningJob};
    use crate::migration;
    use zuno_paths::DbLocation;

    fn fixture() -> (Arc<Pool>, SessionMemoryPolicyStore) {
        let pool = Arc::new(Pool::open(&DbLocation::Memory).expect("open pool"));
        {
            let mut connection = pool.get().expect("connection");
            migration::apply(&mut connection).expect("schema");
            connection
                .execute_batch(
                    "INSERT INTO project
                       (id, worktree, time_created, time_updated, sandboxes)
                     VALUES ('project-1', '/workspace', 1, 1, '[]');
                     INSERT INTO session
                       (id, project_id, slug, directory, title, version, time_created, time_updated)
                     VALUES
                       ('session-1', 'project-1', 'one', '/workspace', 'one', '1', 1, 1),
                       ('session-2', 'project-1', 'two', '/workspace', 'two', '1', 1, 1);
                     INSERT INTO message (id, session_id, time_created, time_updated, data)
                     VALUES
                       ('assistant-1', 'session-1', 1, 1, '{\"role\":\"assistant\"}'),
                       ('assistant-2', 'session-1', 2, 2, '{\"role\":\"assistant\"}'),
                       ('assistant-3', 'session-1', 3, 3, '{\"role\":\"assistant\"}'),
                       ('assistant-4', 'session-2', 4, 4, '{\"role\":\"assistant\"}');",
                )
                .expect("fixture");
        }
        let store = SessionMemoryPolicyStore::new(Arc::clone(&pool));
        (pool, store)
    }

    fn update(
        expected_revision: i64,
        generation: SessionMemoryGeneration,
    ) -> SessionMemoryPolicyUpdate {
        SessionMemoryPolicyUpdate {
            session_id: "session-1".to_owned(),
            use_memories: true,
            generation,
            reason: "user selected the session policy".to_owned(),
            source: "tui".to_owned(),
            expected_revision,
            time_updated: 10 + expected_revision,
        }
    }

    #[test]
    fn missing_rows_use_the_exact_caller_default_without_writing() {
        let (pool, store) = fixture();
        let fallback = SessionMemoryPolicyProjection {
            use_memories: false,
            generation: SessionMemoryGeneration::Disabled,
            reason: Some("headless default".to_owned()),
            source: Some("caller".to_owned()),
            time: Some(7),
            revision: 0,
        };

        assert_eq!(
            store
                .get_or("session-1", fallback.clone())
                .expect("read default"),
            fallback
        );
        let connection = pool.get().expect("connection");
        let rows: i64 = connection
            .query_row("SELECT count(*) FROM session_memory_policy", [], |row| {
                row.get(0)
            })
            .expect("count policy rows");
        assert_eq!(rows, 0);
        assert!(matches!(
            store.get("missing"),
            Err(DbError::NotFound { ref table, .. }) if table == "session"
        ));
    }

    #[test]
    fn child_inheritance_uses_the_parent_callers_default_when_no_row_exists() {
        let (pool, store) = fixture();
        pool.transaction(|transaction| {
            inherit_in(
                transaction,
                "session-1",
                "session-2",
                SessionMemoryPolicyProjection {
                    use_memories: false,
                    generation: SessionMemoryGeneration::Disabled,
                    reason: None,
                    source: None,
                    time: None,
                    revision: 0,
                },
                9,
            )
            .map(|_| ())
        })
        .expect("inherit caller default");

        let child = store
            .get("session-2")
            .expect("child policy")
            .expect("durable child policy");
        assert!(!child.use_memories);
        assert_eq!(child.generation, SessionMemoryGeneration::Disabled);
        assert_eq!(child.source.as_deref(), Some("parent_session"));
    }

    #[test]
    fn set_is_revisioned_and_every_committed_change_is_audited() {
        let (pool, store) = fixture();
        let first = store.set(update(0, SessionMemoryGeneration::Enabled));
        assert!(matches!(
            first.expect("first write"),
            SessionMemoryPolicyWrite::Applied(SessionMemoryPolicyApplied {
                policy: SessionMemoryPolicyProjection { revision: 1, .. },
                skipped_auto_extraction_jobs: 0,
            })
        ));

        let stale = store
            .set(update(0, SessionMemoryGeneration::Disabled))
            .expect("stale write");
        assert!(matches!(
            stale,
            SessionMemoryPolicyWrite::Stale(Some(SessionMemoryPolicyProjection {
                revision: 1,
                ..
            }))
        ));

        let second = store
            .set(update(1, SessionMemoryGeneration::Disabled))
            .expect("second write");
        assert!(matches!(
            second,
            SessionMemoryPolicyWrite::Applied(SessionMemoryPolicyApplied {
                policy: SessionMemoryPolicyProjection {
                    generation: SessionMemoryGeneration::Disabled,
                    revision: 2,
                    ..
                },
                skipped_auto_extraction_jobs: 0,
            })
        ));

        let events = crate::event_log::SessionEventLog::new(pool)
            .read_after("session-1", None)
            .expect("events");
        assert_eq!(events.len(), 2);
        assert!(
            events
                .iter()
                .all(|event| event.event_type == "session.memory.policy.changed")
        );
        assert_eq!(events[0].properties["revision"], json!(1));
        assert_eq!(events[1].properties["previousRevision"], json!(1));
    }

    #[test]
    fn exclude_skips_only_this_sessions_queued_extractions_in_the_same_transaction() {
        let (pool, store) = fixture();
        let jobs = LearningJobStore::new(Arc::clone(&pool));
        for (id, session, message) in [
            ("job-queued-1", "session-1", "assistant-1"),
            ("job-queued-2", "session-1", "assistant-2"),
            ("job-running", "session-1", "assistant-3"),
            ("job-other-session", "session-2", "assistant-4"),
        ] {
            jobs.enqueue(NewLearningJob::extraction(
                id,
                "project-1",
                session,
                message,
                "extractor-v1",
                json!({"transcript":"durable"}),
                5,
            ))
            .expect("enqueue");
        }
        jobs.enqueue(NewLearningJob::extraction(
            "job-manual",
            "project-1",
            "session-1",
            "assistant-1",
            "extractor-manual",
            json!({"trigger":"manual","request":{"transcript":"durable"}}),
            5,
        ))
        .expect("enqueue manual");
        jobs.claim("job-running", "worker", 6, 30)
            .expect("claim running job")
            .expect("running job");

        let outcome = store
            .exclude(SessionMemoryPolicyExclusion {
                session_id: "session-1".to_owned(),
                use_memories: false,
                reason: "external context must not train this session".to_owned(),
                source: "privacy-control".to_owned(),
                expected_revision: 0,
                time_updated: 20,
            })
            .expect("exclude");
        assert!(matches!(
            outcome,
            SessionMemoryPolicyWrite::Applied(SessionMemoryPolicyApplied {
                policy: SessionMemoryPolicyProjection {
                    use_memories: false,
                    generation: SessionMemoryGeneration::Excluded,
                    revision: 1,
                    ..
                },
                skipped_auto_extraction_jobs: 3,
            })
        ));
        for id in ["job-queued-1", "job-queued-2", "job-manual"] {
            let job = jobs.get(id).expect("skipped job");
            assert_eq!(job.status, LearningJobStatus::Skipped);
            assert_eq!(job.time_completed, Some(20));
            assert_eq!(
                job.result.expect("skip result")["kind"],
                json!("sessionMemoryPolicy")
            );
        }
        assert_eq!(
            jobs.get("job-running").expect("running job").status,
            LearningJobStatus::Running
        );
        assert_eq!(
            jobs.get("job-other-session")
                .expect("other session job")
                .status,
            LearningJobStatus::Queued
        );
        let events = crate::event_log::SessionEventLog::new(pool)
            .read_after("session-1", None)
            .expect("events");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].properties["skippedAutoExtractionJobs"], json!(3));

        let error = store
            .set(SessionMemoryPolicyUpdate {
                session_id: "session-1".to_owned(),
                use_memories: true,
                generation: SessionMemoryGeneration::Enabled,
                reason: "try to reopen".to_owned(),
                source: "test".to_owned(),
                expected_revision: 1,
                time_updated: 21,
            })
            .expect_err("excluded generation is terminal for the session");
        assert!(
            error.to_string().contains("cannot be re-enabled"),
            "{error}"
        );
        assert_eq!(
            store
                .get("session-1")
                .expect("policy")
                .expect("stored policy")
                .generation,
            SessionMemoryGeneration::Excluded
        );
    }

    #[test]
    fn disabled_generation_skips_automatic_but_preserves_manual_reflection() {
        let (pool, store) = fixture();
        let jobs = LearningJobStore::new(Arc::clone(&pool));
        jobs.enqueue(NewLearningJob::extraction(
            "job-automatic",
            "project-1",
            "session-1",
            "assistant-1",
            "extractor-automatic",
            json!({
                "trigger":"automatic_post_turn",
                "request":{"transcript":"automatic"}
            }),
            5,
        ))
        .expect("enqueue automatic");
        jobs.enqueue(NewLearningJob::extraction(
            "job-manual",
            "project-1",
            "session-1",
            "assistant-2",
            "extractor-manual",
            json!({"trigger":"manual","request":{"transcript":"manual"}}),
            5,
        ))
        .expect("enqueue manual");

        let outcome = store
            .set(update(0, SessionMemoryGeneration::Disabled))
            .expect("disable generation");
        assert!(matches!(
            outcome,
            SessionMemoryPolicyWrite::Applied(SessionMemoryPolicyApplied {
                skipped_auto_extraction_jobs: 1,
                ..
            })
        ));
        assert_eq!(
            jobs.get("job-automatic").expect("automatic").status,
            LearningJobStatus::Skipped
        );
        assert_eq!(
            jobs.get("job-manual").expect("manual").status,
            LearningJobStatus::Queued
        );
    }

    #[test]
    fn queued_auto_extractions_can_be_skipped_without_creating_a_policy_row() {
        let (pool, store) = fixture();
        let jobs = LearningJobStore::new(Arc::clone(&pool));
        jobs.enqueue(NewLearningJob::extraction(
            "job-queued",
            "project-1",
            "session-1",
            "assistant-1",
            "extractor-v1",
            json!({"transcript":"durable"}),
            5,
        ))
        .expect("enqueue");

        assert_eq!(
            store
                .skip_queued_auto_extraction_jobs(
                    "session-1",
                    "operator excluded prior context",
                    "migration-control",
                    20,
                )
                .expect("skip queued extraction"),
            1
        );
        assert_eq!(
            jobs.get("job-queued").expect("job").status,
            LearningJobStatus::Skipped
        );
        assert_eq!(store.get("session-1").expect("policy lookup"), None);
        assert!(
            crate::event_log::SessionEventLog::new(pool)
                .read_after("session-1", None)
                .expect("events")
                .is_empty()
        );
    }

    #[test]
    fn an_audit_failure_rolls_back_the_policy_write() {
        let (pool, store) = fixture();
        pool.transaction(|transaction| {
            transaction
                .execute_batch("DROP TABLE event")
                .map_err(open::map_error)
        })
        .expect("remove event table");

        store
            .set(update(0, SessionMemoryGeneration::Enabled))
            .expect_err("event append must fail");
        let connection = pool.get().expect("connection");
        let rows: i64 = connection
            .query_row("SELECT count(*) FROM session_memory_policy", [], |row| {
                row.get(0)
            })
            .expect("count policy rows");
        assert_eq!(rows, 0);
    }
}
