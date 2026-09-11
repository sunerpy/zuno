use super::{
    LearningJobKind, LearningJobRecord, LearningJobStatus, MAX_LEARNING_JOB_ATTEMPTS,
    NewLearningJob, read_required,
};
use crate::{
    Connection,
    learning_source::{LearningSource, LearningSourceSnapshot, snapshot_current_on},
    open,
};
use rusqlite::params;
use serde_json::Value;
use zuno_error::DbError;

pub(super) fn current_on(
    connection: &Connection,
    job: &LearningJobRecord,
) -> Result<bool, DbError> {
    if job.kind == LearningJobKind::Extraction {
        let forgotten: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM experience_record
             WHERE session_id=?1 AND source_message_id=?2 AND status='forgotten')",
                params![job.session_id, job.source_message_id],
                |row| row.get(0),
            )
            .map_err(open::map_error)?;
        if forgotten {
            return Ok(false);
        }
    }
    validate_on(
        connection,
        job.kind,
        job.project_id.as_deref(),
        job.session_id.as_deref(),
        job.source_message_id.as_deref(),
        job.payload.as_ref(),
    )
}

fn validate_on(
    connection: &Connection,
    kind: LearningJobKind,
    project_id: Option<&str>,
    session_id: Option<&str>,
    message_id: Option<&str>,
    payload: Option<&Value>,
) -> Result<bool, DbError> {
    let Some(value) = payload.and_then(|value| value.get("sourceSnapshot")) else {
        // Legacy jobs never gain the busy-session exception from this result.
        return Ok(true);
    };
    let Ok(snapshot) = serde_json::from_value::<LearningSourceSnapshot>(value.clone()) else {
        return Ok(false);
    };
    let request = &payload.expect("snapshot has a payload")["request"];
    if kind != LearningJobKind::Extraction
        || project_id != Some(snapshot.project_id.as_str())
        || session_id != Some(snapshot.session_id.as_str())
        || message_id != Some(snapshot.source_message_id.as_str())
        || request["project_id"].as_str() != project_id
        || request["session_id"].as_str() != session_id
        || request["source_message_id"].as_str() != message_id
    {
        return Ok(false);
    }
    let Ok(sources) = serde_json::from_value::<Vec<LearningSource>>(request["sources"].clone())
    else {
        return Ok(false);
    };
    snapshot_current_on(connection, &snapshot, &sources)
}

pub(super) fn validate_new_on(
    connection: &Connection,
    job: &NewLearningJob,
) -> Result<(), DbError> {
    if !validate_on(
        connection,
        job.kind,
        job.project_id.as_deref(),
        job.session_id.as_deref(),
        job.source_message_id.as_deref(),
        job.payload.as_ref(),
    )? {
        return Err(DbError::Conflict {
            table: "learning_job".to_owned(),
            id: job.id.clone(),
            detail: "learning source snapshot changed before admission".to_owned(),
        });
    }
    Ok(())
}

pub(super) fn validate_queued_on(
    connection: &Connection,
    id: &str,
    now: i64,
) -> Result<bool, DbError> {
    let job = match read_required(connection, id) {
        Ok(job) => job,
        Err(DbError::NotFound { .. }) => return Ok(false),
        Err(error) => return Err(error),
    };
    if job.status != LearningJobStatus::Queued {
        return Ok(false);
    }
    if job.attempt >= MAX_LEARNING_JOB_ATTEMPTS {
        connection.execute(
            "UPDATE learning_job SET status='failed',owner_id=NULL,lease_token=NULL,lease_expires=NULL,
               error=COALESCE(error,'learning attempt limit reached'),time_updated=?2,time_completed=?2
             WHERE id=?1 AND status='queued'",
            params![id,now],
        ).map_err(open::map_error)?;
        return Ok(false);
    }
    if current_on(connection, &job)? {
        return Ok(true);
    }
    connection.execute(
        "UPDATE learning_job SET status='skipped',
           error='closed source snapshot is unavailable or changed',time_updated=?2,time_completed=?2
         WHERE id=?1 AND status='queued'",
        params![id,now],
    ).map_err(open::map_error)?;
    Ok(false)
}
