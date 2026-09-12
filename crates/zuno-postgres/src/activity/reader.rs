use super::*;
use async_trait::async_trait;
use zuno_application::activity::{ActivityPersistence, FrameQuery, HistoryQuery};

#[async_trait]
impl ActivityPersistence for PostgresActivityPersistence {
    async fn history(
        &self,
        session: &SessionId,
        request: HistoryQuery,
    ) -> Result<HistoryPage, ApplicationError> {
        let owner = self.principal.owner();
        let mut tx = crate::scoped_transaction(&self.backend.pool, &self.principal).await?;
        crate::session::read_session(&mut tx, &self.principal, session.as_str(), false).await?;
        let latest = current(&mut tx, &owner, session).await?;
        let through = request.through.unwrap_or(counter(latest)?);
        if integer(through)? > latest {
            return Err(ApplicationError::Conflict);
        }
        let rows = query(
            "SELECT i.position,f.sequence,f.record FROM zuno_enterprise_preview.activity_item i
             JOIN LATERAL(
               SELECT sequence,record FROM zuno_enterprise_preview.activity_frame f
               WHERE f.tenant_id=i.tenant_id AND f.principal_id=i.principal_id AND f.session_id=i.session_id
                 AND f.item_id=i.id AND f.sequence<=$4 ORDER BY f.sequence DESC LIMIT 1
             ) f ON true
             WHERE i.tenant_id=$1 AND i.principal_id=$2 AND i.session_id=$3 AND i.position<=$4
               AND ($5::bigint IS NULL OR i.position<$5)
             ORDER BY i.position DESC LIMIT $6",
        ).bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(session.as_str())
            .bind(integer(through)?).bind(request.before.map(integer).transpose()?).bind(i64::from(request.limit.get())+1)
            .fetch_all(&mut *tx).await.map_err(database_error)?;
        let available = rows.len();
        let mut items = Vec::new();
        let mut bytes = 512usize;
        for row in rows.into_iter().take(request.limit.get() as usize) {
            let record: ItemRecord =
                serde_json::from_value(row.try_get("record").map_err(database_error)?)
                    .map_err(ApplicationError::storage)?;
            let item = HistoryItem {
                position: counter(row.try_get("position").map_err(database_error)?)?,
                revision: counter(row.try_get("sequence").map_err(database_error)?)?,
                record,
            };
            let size = serde_json::to_vec(&item)
                .map_err(ApplicationError::storage)?
                .len()
                + 1;
            if bytes + size > MAX_ACTIVITY_PAGE_BYTES {
                if items.is_empty() {
                    return Err(ApplicationError::Invalid(
                        "activity item exceeds page bound".to_owned(),
                    ));
                }
                break;
            }
            bytes += size;
            items.push(item);
        }
        let before = (items.len() < available)
            .then(|| items.last().map(|item| item.position))
            .flatten();
        // Pages are selected newest-first but displayed in chronological order.
        items.reverse();
        tx.commit().await.map_err(database_error)?;
        Ok(HistoryPage {
            version: ACTIVITY_PROTOCOL_VERSION,
            session_id: session.clone(),
            through,
            items,
            before,
        })
    }

    async fn frames(
        &self,
        session: &SessionId,
        request: FrameQuery,
    ) -> Result<FramePage, ApplicationError> {
        let owner = self.principal.owner();
        let mut tx = crate::scoped_transaction(&self.backend.pool, &self.principal).await?;
        crate::session::read_session(&mut tx, &self.principal, session.as_str(), false).await?;
        let latest = current(&mut tx, &owner, session).await?;
        if integer(request.after)? > latest {
            return Err(ApplicationError::Conflict);
        }
        let rows = query(
            "SELECT f.sequence,f.version,f.record,i.position FROM zuno_enterprise_preview.activity_frame f
             JOIN zuno_enterprise_preview.activity_item i
               ON i.tenant_id=f.tenant_id AND i.principal_id=f.principal_id AND i.session_id=f.session_id AND i.id=f.item_id
             WHERE f.tenant_id=$1 AND f.principal_id=$2 AND f.session_id=$3 AND f.sequence>$4 AND f.sequence<=$5
             ORDER BY f.sequence LIMIT $6",
        ).bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(session.as_str())
            .bind(integer(request.after)?).bind(latest).bind(i64::from(request.limit.get()))
            .fetch_all(&mut *tx).await.map_err(database_error)?;
        let mut frames = Vec::new();
        let mut through = request.after;
        let mut bytes = 512usize;
        for row in rows {
            let version: i32 = row.try_get("version").map_err(database_error)?;
            if version != ACTIVITY_PROTOCOL_VERSION as i32 {
                return Err(ApplicationError::Invalid(
                    "unsupported activity frame".to_owned(),
                ));
            }
            let frame = CommittedFrame {
                version: ACTIVITY_PROTOCOL_VERSION,
                session_id: session.clone(),
                sequence: counter(row.try_get("sequence").map_err(database_error)?)?,
                event: CommittedEvent::Upsert {
                    position: counter(row.try_get("position").map_err(database_error)?)?,
                    record: Box::new(
                        serde_json::from_value(row.try_get("record").map_err(database_error)?)
                            .map_err(ApplicationError::storage)?,
                    ),
                },
            };
            let size = serde_json::to_vec(&frame)
                .map_err(ApplicationError::storage)?
                .len()
                + 1;
            if bytes + size > MAX_ACTIVITY_PAGE_BYTES {
                if frames.is_empty() {
                    return Err(ApplicationError::Invalid(
                        "activity frame exceeds page bound".to_owned(),
                    ));
                }
                break;
            }
            bytes += size;
            through = frame.sequence;
            frames.push(frame);
        }
        tx.commit().await.map_err(database_error)?;
        Ok(FramePage {
            version: ACTIVITY_PROTOCOL_VERSION,
            session_id: session.clone(),
            more: through.0 < latest as u64,
            through,
            frames,
        })
    }
}

async fn current(
    connection: &mut PgConnection,
    owner: &PrincipalKey,
    session: &SessionId,
) -> Result<i64, ApplicationError> {
    query_scalar("SELECT sequence FROM zuno_enterprise_preview.activity_session WHERE tenant_id=$1 AND principal_id=$2 AND session_id=$3")
        .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(session.as_str())
        .fetch_one(connection).await.map_err(database_error)
}
