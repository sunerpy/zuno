//! Private session ownership, independent of caller-editable metadata.

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use zuno_error::DbError;
use zuno_types::identity::{PrincipalId, PrincipalKey, TenantId};

use crate::open::map_error;

/// A private-session consumer that cannot drop its caller's ownership filter.
pub struct ScopedSessionStore {
    pool: std::sync::Arc<crate::Pool>,
    owner: PrincipalKey,
}

impl ScopedSessionStore {
    #[must_use]
    pub fn new(pool: std::sync::Arc<crate::Pool>, owner: PrincipalKey) -> Self {
        Self { pool, owner }
    }

    pub fn create(
        &self,
        input: &crate::session::SessionCreate,
    ) -> Result<crate::session::Creation, DbError> {
        let mut input = input.clone();
        if input
            .owner
            .as_ref()
            .is_some_and(|owner| owner != &self.owner)
        {
            return Err(DbError::Conflict {
                table: "session_ownership".to_owned(),
                id: input.id,
                detail: "a scoped store cannot create another principal's session".to_owned(),
            });
        }
        input.owner = Some(self.owner.clone());
        self.pool
            .transaction(|tx| crate::session::create(tx, &input))
    }

    pub fn get(&self, session_id: &str) -> Result<crate::session::Session, DbError> {
        let connection = self.pool.get()?;
        crate::session::get_owned(&connection, session_id, &self.owner)
    }

    pub fn list(
        &self,
        query: &crate::session::ListQuery,
    ) -> Result<Vec<crate::session::Session>, DbError> {
        let connection = self.pool.get()?;
        let mut query = query.clone();
        query.owner = Some(self.owner.clone());
        crate::session::list(&connection, &query)
    }
}

/// Load and validate a durable ownership record.
pub fn get(connection: &Connection, session_id: &str) -> Result<PrincipalKey, DbError> {
    let row = connection
        .query_row(
            "SELECT tenant_id,principal_id FROM session_ownership WHERE session_id=?1",
            [session_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()
        .map_err(map_error)?
        .ok_or_else(|| DbError::NotFound {
            table: "session_ownership".to_owned(),
            id: session_id.to_owned(),
        })?;
    let invalid = |source| DbError::Query {
        source: Box::new(source),
    };
    Ok(PrincipalKey {
        tenant_id: TenantId::new(row.0).map_err(invalid)?,
        principal_id: PrincipalId::new(row.1).map_err(invalid)?,
    })
}

/// Bind a newly inserted session inside its creation transaction.
///
/// Existing sessions can only be checked, never reassigned through creation.
pub(crate) fn bind_created(
    transaction: &Transaction<'_>,
    session_id: &str,
    parent_id: Option<&str>,
    owner: &PrincipalKey,
    inserted: bool,
) -> Result<(), DbError> {
    let conflict = || DbError::Conflict {
        table: "session_ownership".to_owned(),
        id: session_id.to_owned(),
        detail: "session ownership does not match the requested principal".to_owned(),
    };
    if let Some(parent) = parent_id
        && get(transaction, parent)? != *owner
    {
        return Err(conflict());
    }
    if !inserted {
        return if get(transaction, session_id)? == *owner {
            Ok(())
        } else {
            Err(conflict())
        };
    }
    let changed = transaction
        .execute(
            "UPDATE session_ownership SET tenant_id=?2,principal_id=?3 WHERE session_id=?1",
            params![
                session_id,
                owner.tenant_id.as_str(),
                owner.principal_id.as_str()
            ],
        )
        .map_err(map_error)?;
    if changed != 1 {
        return Err(conflict());
    }
    Ok(())
}
