//! Tenant-owned Memory spaces. Current actor authority and namespace ownership
//! remain separate throughout the transaction.
mod evidence;
mod mutations;
mod support;
use crate::{PostgresBackend, database_error, database_time, owner_transaction};
use async_trait::async_trait;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use sqlx_core::{query::query, query_scalar::query_scalar, row::Row, transaction::Transaction};
use sqlx_postgres::Postgres;
use zuno_application::{ApplicationError, PageSize, shared_memory::*};
use zuno_permission::enterprise::{OrganizationRole, actor_denial};
use zuno_types::{
    activity::Counter,
    identity::{MemorySpaceId, PrincipalScope, RequestId, WorkspaceId},
};

type Tx<'a> = Transaction<'a, Postgres>;
#[derive(Clone)]
pub struct PostgresSharedMemoryStore {
    backend: PostgresBackend,
}
impl PostgresBackend {
    pub fn shared_memory(&self) -> PostgresSharedMemoryStore {
        PostgresSharedMemoryStore {
            backend: self.clone(),
        }
    }
}
pub(crate) async fn snapshots(
    tx: &mut Tx<'_>,
    principal: &PrincipalScope,
    workspace: &WorkspaceId,
) -> Result<zuno_memory::remote::MemoryReply, ApplicationError> {
    actor(tx, principal, false).await?;
    // Model recall requires explicit membership even when its owner can audit
    // every namespace as an organization administrator.
    let ids:Vec<String>=query_scalar("SELECT s.id FROM zuno_enterprise_preview.shared_memory_space s
        JOIN zuno_enterprise_preview.shared_memory_member m ON m.tenant_id=s.tenant_id AND m.space_id=s.id
        WHERE s.tenant_id=$1 AND s.workspace_id=$2 AND s.enabled AND m.principal_id=$3 ORDER BY s.id LIMIT 33")
        .bind(principal.tenant_id().as_str()).bind(workspace.as_str()).bind(principal.principal_id().as_str())
        .fetch_all(&mut **tx).await.map_err(database_error)?;
    if ids.len() > 32 {
        return Err(ApplicationError::Invalid(
            "too many shared Memory spaces for this workspace".to_owned(),
        ));
    }
    let mut documents = Vec::new();
    let mut omitted_spaces = Vec::new();
    let mut bytes = 0usize;
    for id in ids {
        let id = MemorySpaceId::new(id).map_err(ApplicationError::storage)?;
        let space = match read(tx, principal, &id, false).await {
            Ok(space) => space,
            Err(ApplicationError::NotFound) => continue,
            Err(error) => return Err(error),
        };
        let entries = space
            .entries
            .into_iter()
            .filter(|entry| !space.suppressed.contains(entry))
            .collect::<Vec<_>>();
        if entries.is_empty() {
            continue;
        }
        let content = format!(
            "MEMORY (organization shared: {})\n{}\n",
            space.title,
            zuno_memory::render::serialize(&entries)
        );
        if bytes + content.len() > 65536 {
            omitted_spaces.push(id);
            continue;
        }
        bytes += content.len();
        documents.push(zuno_memory::remote::SharedMemorySnapshot {
            space_id: id,
            title: space.title,
            revision: space.document_revision.0,
            digest: zuno_orchestration::sha256_text(&content),
            content,
        });
    }
    Ok(zuno_memory::remote::MemoryReply::SharedSnapshot {
        documents,
        omitted_spaces,
    })
}
fn invalid() -> ApplicationError {
    ApplicationError::Invalid("invalid shared Memory state or request".to_owned())
}
fn number(value: u64) -> Result<i64, ApplicationError> {
    i64::try_from(value).map_err(|_| invalid())
}
fn counter(value: i64) -> Result<Counter, ApplicationError> {
    u64::try_from(value).map(Counter).map_err(|_| invalid())
}
fn text<T: Serialize>(value: &T) -> Result<String, ApplicationError> {
    serde_json::to_value(value)
        .map_err(ApplicationError::storage)?
        .as_str()
        .map(str::to_owned)
        .ok_or_else(invalid)
}
fn digest<T: Serialize>(value: &T) -> String {
    zuno_orchestration::sha256_json(&json!(value))
}
async fn actor(
    tx: &mut Tx<'_>,
    principal: &PrincipalScope,
    review: bool,
) -> Result<OrganizationRole, ApplicationError> {
    let access = crate::authorization::access_in(tx, &principal.owner()).await?;
    if actor_denial(&access.policy, &access.member, principal).is_some()
        || (review
            && (principal.kind() != zuno_types::identity::PrincipalKind::User
                || principal
                    .client_id()
                    .is_none_or(|id| !access.policy.approval_apps.contains(id))))
    {
        return Err(ApplicationError::Forbidden);
    }
    Ok(access.member.role)
}
async fn lock(
    tx: &mut Tx<'_>,
    principal: &PrincipalScope,
    id: &MemorySpaceId,
) -> Result<(), ApplicationError> {
    query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
        .bind(digest(&json!(["shared-memory", principal.tenant_id(), id])))
        .execute(&mut **tx)
        .await
        .map_err(database_error)?;
    Ok(())
}
async fn role(
    tx: &mut Tx<'_>,
    principal: &PrincipalScope,
    id: &MemorySpaceId,
    admin: bool,
) -> Result<SharedMemoryRole, ApplicationError> {
    let value: Option<String> = query_scalar(
        "SELECT role FROM zuno_enterprise_preview.shared_memory_member
        WHERE tenant_id=$1 AND space_id=$2 AND principal_id=$3",
    )
    .bind(principal.tenant_id().as_str())
    .bind(id.as_str())
    .bind(principal.principal_id().as_str())
    .fetch_optional(&mut **tx)
    .await
    .map_err(database_error)?;
    match value {
        Some(value) => {
            serde_json::from_value(Value::String(value)).map_err(ApplicationError::storage)
        }
        None if admin => Ok(SharedMemoryRole::Reader),
        None => Err(ApplicationError::NotFound),
    }
}
async fn read(
    tx: &mut Tx<'_>,
    principal: &PrincipalScope,
    id: &MemorySpaceId,
    admin: bool,
) -> Result<SharedMemorySpace, ApplicationError> {
    let role = role(tx, principal, id, admin).await?;
    let row=query("SELECT workspace_id,title,enabled,policy_revision,document_revision,entries,content_digest,character_limit
        FROM zuno_enterprise_preview.shared_memory_space WHERE tenant_id=$1 AND id=$2")
        .bind(principal.tenant_id().as_str()).bind(id.as_str()).fetch_optional(&mut **tx).await.map_err(database_error)?
        .ok_or(ApplicationError::NotFound)?;
    let enabled: bool = row.try_get("enabled").map_err(database_error)?;
    if !enabled && !admin {
        return Err(ApplicationError::NotFound);
    }
    let entries: Vec<String> =
        serde_json::from_value(row.try_get("entries").map_err(database_error)?)
            .map_err(ApplicationError::storage)?;
    let hash: String = row.try_get("content_digest").map_err(database_error)?;
    if digest(&entries) != hash {
        return Err(ApplicationError::Conflict);
    }
    let suppressed = support::suppressed(tx, principal, id, &entries).await?;
    Ok(SharedMemorySpace {
        id: id.clone(),
        workspace_id: WorkspaceId::new(
            row.try_get::<String, _>("workspace_id")
                .map_err(database_error)?,
        )
        .map_err(ApplicationError::storage)?,
        title: row.try_get("title").map_err(database_error)?,
        enabled,
        role,
        policy_revision: counter(row.try_get("policy_revision").map_err(database_error)?)?,
        document_revision: counter(row.try_get("document_revision").map_err(database_error)?)?,
        entries,
        digest: hash,
        character_limit: u32::try_from(
            row.try_get::<i32, _>("character_limit")
                .map_err(database_error)?,
        )
        .map_err(|_| invalid())?,
        suppressed,
    })
}
async fn prior<T: DeserializeOwned>(
    tx: &mut Tx<'_>,
    principal: &PrincipalScope,
    id: &RequestId,
    hash: &str,
) -> Result<Option<T>, ApplicationError> {
    let row = query(
        "SELECT request_digest,response FROM zuno_enterprise_preview.shared_memory_request
        WHERE tenant_id=$1 AND principal_id=$2 AND request_id=$3",
    )
    .bind(principal.tenant_id().as_str())
    .bind(principal.principal_id().as_str())
    .bind(id.as_str())
    .fetch_optional(&mut **tx)
    .await
    .map_err(database_error)?;
    row.map(|row| {
        if row
            .try_get::<String, _>("request_digest")
            .map_err(database_error)?
            != hash
        {
            return Err(ApplicationError::Conflict);
        }
        serde_json::from_value(row.try_get("response").map_err(database_error)?)
            .map_err(ApplicationError::storage)
    })
    .transpose()
}
async fn receipt<T: Serialize>(
    tx: &mut Tx<'_>,
    principal: &PrincipalScope,
    id: &MemorySpaceId,
    request: &RequestId,
    operation: &str,
    response: &T,
    hash: &str,
) -> Result<(), ApplicationError> {
    query("INSERT INTO zuno_enterprise_preview.shared_memory_request(tenant_id,principal_id,request_id,request_digest,response)
        VALUES($1,$2,$3,$4,$5)")
        .bind(principal.tenant_id().as_str()).bind(principal.principal_id().as_str()).bind(request.as_str()).bind(hash)
        .bind(json!(response)).execute(&mut **tx).await.map_err(database_error)?;
    let now = database_time(tx).await?;
    query("INSERT INTO zuno_enterprise_preview.shared_memory_audit(tenant_id,space_id,id,actor,operation,data,time_created)
        VALUES($1,$2,$3,$4,$5,$6,$7)")
        .bind(principal.tenant_id().as_str()).bind(id.as_str()).bind(uuid::Uuid::now_v7().to_string())
        .bind(json!(principal)).bind(operation).bind(json!({"requestId":request,"requestDigest":hash,"result":response})).bind(now)
        .execute(&mut **tx).await.map_err(database_error)?;
    Ok(())
}
fn change_digest(change: &SharedMemoryChange) -> String {
    let mut value = change.clone();
    value.state_digest.clear();
    digest(&value)
}
async fn change(
    tx: &mut Tx<'_>,
    principal: &PrincipalScope,
    id: &MemorySpaceId,
    candidate: &RequestId,
) -> Result<SharedMemoryChange, ApplicationError> {
    let row = query(
        "SELECT data,state_digest FROM zuno_enterprise_preview.shared_memory_change
        WHERE tenant_id=$1 AND space_id=$2 AND id=$3",
    )
    .bind(principal.tenant_id().as_str())
    .bind(id.as_str())
    .bind(candidate.as_str())
    .fetch_optional(&mut **tx)
    .await
    .map_err(database_error)?
    .ok_or(ApplicationError::NotFound)?;
    let value: SharedMemoryChange =
        serde_json::from_value(row.try_get("data").map_err(database_error)?)
            .map_err(ApplicationError::storage)?;
    if value.space_id != *id
        || value.id != *candidate
        || value.state_digest != change_digest(&value)
        || value.state_digest
            != row
                .try_get::<String, _>("state_digest")
                .map_err(database_error)?
    {
        return Err(ApplicationError::Conflict);
    }
    Ok(value)
}
#[async_trait]
impl SharedMemoryStore for PostgresSharedMemoryStore {
    async fn configure(
        &self,
        principal: &PrincipalScope,
        id: &MemorySpaceId,
        request: ConfigureSharedMemory,
    ) -> Result<SharedMemorySpace, ApplicationError> {
        self.configure_space(principal, id, request).await
    }
    async fn read(
        &self,
        principal: &PrincipalScope,
        id: &MemorySpaceId,
    ) -> Result<SharedMemorySpace, ApplicationError> {
        let mut tx = owner_transaction(&self.backend.pool, &principal.owner()).await?;
        let admin = actor(&mut tx, principal, false).await? == OrganizationRole::Administrator;
        let value = read(&mut tx, principal, id, admin).await?;
        tx.commit().await.map_err(database_error)?;
        Ok(value)
    }
    async fn list(
        &self,
        principal: &PrincipalScope,
        workspace: &WorkspaceId,
        after: Option<&MemorySpaceId>,
        limit: PageSize,
    ) -> Result<SharedMemoryPage, ApplicationError> {
        let mut tx = owner_transaction(&self.backend.pool, &principal.owner()).await?;
        let admin = actor(&mut tx, principal, false).await? == OrganizationRole::Administrator;
        let rows:Vec<String>=query_scalar("SELECT id FROM zuno_enterprise_preview.shared_memory_space
            WHERE tenant_id=$1 AND workspace_id=$2 AND enabled AND ($3::text IS NULL OR id>$3) ORDER BY id LIMIT $4")
            .bind(principal.tenant_id().as_str()).bind(workspace.as_str()).bind(after.map(MemorySpaceId::as_str))
            .bind(i64::from(limit.get())+1).fetch_all(&mut *tx).await.map_err(database_error)?;
        let mut more = rows.len() > usize::from(limit.get());
        let mut items = Vec::new();
        let mut bytes = 0usize;
        for id in rows.into_iter().take(usize::from(limit.get())) {
            let item = read(
                &mut tx,
                principal,
                &MemorySpaceId::new(id).map_err(ApplicationError::storage)?,
                admin,
            )
            .await?;
            let size = serde_json::to_vec(&item)
                .map_err(ApplicationError::storage)?
                .len();
            if bytes + size > 524288 {
                more = true;
                break;
            }
            bytes += size;
            items.push(item);
        }
        let after = more
            .then(|| items.last().map(|space| space.id.clone()))
            .flatten();
        tx.commit().await.map_err(database_error)?;
        Ok(SharedMemoryPage { items, after })
    }
    async fn propose(
        &self,
        principal: &PrincipalScope,
        id: &MemorySpaceId,
        request: ProposeSharedMemory,
    ) -> Result<SharedMemoryChange, ApplicationError> {
        self.propose_change(principal, id, request).await
    }
    async fn change(
        &self,
        principal: &PrincipalScope,
        id: &MemorySpaceId,
        candidate: &RequestId,
    ) -> Result<SharedMemoryChange, ApplicationError> {
        let mut tx = owner_transaction(&self.backend.pool, &principal.owner()).await?;
        let admin = actor(&mut tx, principal, false).await? == OrganizationRole::Administrator;
        read(&mut tx, principal, id, admin).await?;
        let value = change(&mut tx, principal, id, candidate).await?;
        tx.commit().await.map_err(database_error)?;
        Ok(value)
    }
    async fn review(
        &self,
        principal: &PrincipalScope,
        id: &MemorySpaceId,
        request: ReviewSharedMemory,
    ) -> Result<SharedMemoryChange, ApplicationError> {
        self.review_change(principal, id, request).await
    }
}
