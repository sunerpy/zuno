use super::*;
use zuno_engine::context_usage::ContextUsageSeed;
use zuno_types::context_usage::{
    ContextUsageSnapshot, ContextUsageSource, ContextUsageTotals, ContextUsageTracker,
    ContextUsageWrite,
};

fn number(value: i64) -> Result<u64, TurnError> {
    u64::try_from(value).map_err(|_| TurnStateError::InvalidData.into())
}

pub(super) async fn read(
    tx: &mut Transaction<'_, Postgres>,
    scope: &TurnStateScope,
    source: ContextUsageSource,
) -> Result<Option<ContextUsageTracker>, TurnError> {
    let row = query(
        "SELECT revision,context_epoch,state,time_updated FROM zuno_enterprise_preview.context_usage
         WHERE tenant_id=$1 AND principal_id=$2 AND session_id=$3 AND source=$4",
    ).bind(scope.owner.tenant_id.as_str()).bind(scope.owner.principal_id.as_str()).bind(&scope.session_id)
        .bind(source.as_str()).fetch_optional(&mut **tx).await.map_err(sql_error)?;
    row.map(|row| {
        let tracker: ContextUsageTracker =
            serde_json::from_value(row.try_get("state").map_err(sql_error)?)
                .map_err(|_| TurnStateError::InvalidData)?;
        tracker
            .validate()
            .map_err(|_| TurnStateError::InvalidData)?;
        let snapshot = tracker.snapshot();
        if snapshot.session_id != scope.session_id
            || snapshot.source != source
            || snapshot.revision != number(row.try_get("revision").map_err(sql_error)?)?
            || snapshot.context_epoch != number(row.try_get("context_epoch").map_err(sql_error)?)?
            || snapshot.time_updated != row.try_get::<i64, _>("time_updated").map_err(sql_error)?
        {
            return Err(TurnStateError::InvalidData.into());
        }
        Ok(tracker)
    })
    .transpose()
}

pub(super) async fn seed(
    tx: &mut Transaction<'_, Postgres>,
    scope: &TurnStateScope,
) -> Result<ContextUsageSeed, TurnError> {
    let session = query(
        "SELECT * FROM zuno_enterprise_preview.session WHERE tenant_id=$1 AND principal_id=$2 AND id=$3",
    ).bind(scope.owner.tenant_id.as_str()).bind(scope.owner.principal_id.as_str()).bind(&scope.session_id)
        .fetch_one(&mut **tx).await.map_err(sql_error)?;
    let source = if session
        .try_get::<Option<String>, _>("parent_id")
        .map_err(sql_error)?
        .is_some()
    {
        ContextUsageSource::Child
    } else {
        ContextUsageSource::Main
    };
    if let Some(tracker) = read(tx, scope, source).await? {
        return Ok(ContextUsageSeed {
            persisted_revision: Some(tracker.snapshot().revision),
            tracker,
        });
    }
    // Before canonical tracking existed, aggregate consumption is authoritative
    // but occupancy is not. Preserve the total without inventing a context window.
    let mut snapshot = ContextUsageSnapshot::unknown(&scope.session_id);
    snapshot.source = source;
    snapshot.context_epoch = number(session.try_get("context_epoch").map_err(sql_error)?)?;
    snapshot.context_limit = session
        .try_get::<Option<i64>, _>("tokens_context_limit")
        .map_err(sql_error)?
        .and_then(|value| u64::try_from(value).ok())
        .filter(|value| *value > 0);
    snapshot.cumulative_usage = ContextUsageTotals {
        input: number(session.try_get("tokens_input").map_err(sql_error)?)?,
        output: number(session.try_get("tokens_output").map_err(sql_error)?)?,
        reasoning: number(session.try_get("tokens_reasoning").map_err(sql_error)?)?,
        cache_read: number(session.try_get("tokens_cache_read").map_err(sql_error)?)?,
        cache_write: number(session.try_get("tokens_cache_write").map_err(sql_error)?)?,
        unclassified: 0,
    };
    let no_attempt: bool = query_scalar(
        "SELECT NOT EXISTS(SELECT 1 FROM zuno_enterprise_preview.event WHERE tenant_id=$1 AND principal_id=$2
          AND session_id=$3 AND type='session.provider.attempt')
         AND NOT EXISTS(SELECT 1 FROM zuno_enterprise_preview.message WHERE tenant_id=$1 AND principal_id=$2 AND session_id=$3 AND role='assistant')",
    ).bind(scope.owner.tenant_id.as_str()).bind(scope.owner.principal_id.as_str()).bind(&scope.session_id)
        .fetch_one(&mut **tx).await.map_err(sql_error)?;
    snapshot.cumulative_known = session
        .try_get::<bool, _>("tokens_known")
        .map_err(sql_error)?
        || no_attempt;
    let tracker =
        ContextUsageTracker::from_snapshot(snapshot).map_err(|_| TurnStateError::InvalidData)?;
    Ok(ContextUsageSeed {
        tracker,
        persisted_revision: None,
    })
}

pub(super) async fn write(
    tx: &mut Transaction<'_, Postgres>,
    scope: &TurnStateScope,
    job: &RuntimeJob,
    update: &ContextUsageWrite,
) -> Result<(), TurnError> {
    let snapshot = update.tracker.snapshot();
    if snapshot.session_id != scope.session_id {
        return Err(TurnStateError::Conflict.into());
    }
    let current = read(tx, scope, snapshot.source).await?;
    if !zuno_db::context_usage::validate_update(
        current.as_ref(),
        update.expected_revision,
        &update.tracker,
    )
    .map_err(|error| match error {
        zuno_error::DbError::Conflict { .. } => TurnStateError::Conflict,
        _ => TurnStateError::InvalidData,
    })? {
        return Ok(());
    }
    let revision = i64::try_from(snapshot.revision).map_err(|_| TurnStateError::InvalidData)?;
    let epoch = i64::try_from(snapshot.context_epoch).map_err(|_| TurnStateError::InvalidData)?;
    let expected = update
        .expected_revision
        .map(i64::try_from)
        .transpose()
        .map_err(|_| TurnStateError::InvalidData)?;
    let changed = query(
        "INSERT INTO zuno_enterprise_preview.context_usage(tenant_id,principal_id,session_id,source,revision,context_epoch,state,time_updated)
         VALUES($1,$2,$3,$4,$5,$6,$7,$8)
         ON CONFLICT(tenant_id,principal_id,session_id,source) DO UPDATE SET
           revision=excluded.revision,context_epoch=excluded.context_epoch,state=excluded.state,time_updated=excluded.time_updated
         WHERE context_usage.revision=$9 AND context_usage.context_epoch<=excluded.context_epoch AND context_usage.time_updated<=excluded.time_updated",
    ).bind(scope.owner.tenant_id.as_str()).bind(scope.owner.principal_id.as_str()).bind(&scope.session_id)
        .bind(snapshot.source.as_str()).bind(revision).bind(epoch).bind(json!(update.tracker)).bind(snapshot.time_updated)
        .bind(expected).execute(&mut **tx).await.map_err(sql_error)?.rows_affected();
    if changed != 1 {
        return Err(TurnStateError::Conflict.into());
    }
    event(
        tx,
        job,
        NewSessionEvent::new(
            "session.context.usage",
            json!({"snapshot":snapshot})
                .as_object()
                .expect("fixed envelope")
                .clone(),
        )?,
    )
    .await?;
    Ok(())
}
