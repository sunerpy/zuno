use crate::storage;
mod forks;
mod merges;
pub(crate) use forks::ForkPreparation;
pub(crate) use merges::MergeRecord;
const SCHEMA: &str = include_str!("schema.sql");
const DELIVERY_SCHEMA: &str = include_str!("schema_delivery.sql");
const FORK_SCHEMA: &str = include_str!("schema_fork.sql");
const MERGE_SCHEMA: &str = include_str!("schema_merge.sql");
const FORMAT: i64 = 4;

fn source_digest(version: i64) -> String {
    if version == 1 {
        zuno_orchestration::sha256_text(SCHEMA)
    } else if version == 2 {
        zuno_orchestration::sha256_text(&format!("{SCHEMA}\n{DELIVERY_SCHEMA}"))
    } else if version == 3 {
        zuno_orchestration::sha256_text(&format!("{SCHEMA}\n{DELIVERY_SCHEMA}\n{FORK_SCHEMA}"))
    } else {
        zuno_orchestration::sha256_text(&format!(
            "{SCHEMA}\n{DELIVERY_SCHEMA}\n{FORK_SCHEMA}\n{MERGE_SCHEMA}"
        ))
    }
}

fn manifest(connection: &Connection) -> Result<String, ApplicationError> {
    let mut query=connection.prepare(
        "SELECT type,name,tbl_name,sql FROM sqlite_master WHERE name NOT LIKE 'sqlite_%' AND name<>'gateway_format' ORDER BY type,name",
    ).map_err(storage)?;
    let rows = query
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })
        .map_err(storage)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(storage)?;
    Ok(zuno_orchestration::sha256_json(&serde_json::json!(rows)))
}
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Mutex;
use zuno_application::ApplicationError;
use zuno_application::environment::EnvironmentSnapshot;
use zuno_application::environment::{
    CommandOperation, Environment, EnvironmentSpec, OperationCompletion, OperationPhase,
    OperationReceipt,
};
use zuno_application::runtime::ExecutionLease;
use zuno_types::identity::EnvironmentSnapshotId;
use zuno_types::identity::{EnvironmentId, OperationId, PrincipalKey};

pub(crate) struct Ledger {
    connection: Mutex<Connection>,
    _instance: std::fs::File,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Operation {
    pub owner: PrincipalKey,
    pub lease: ExecutionLease,
    pub request: CommandOperation,
    pub container: String,
    pub receipt: OperationReceipt,
}

impl Ledger {
    pub(crate) fn open(path: &Path) -> Result<Self, ApplicationError> {
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path.with_extension("lock"))
            .map_err(storage)?;
        lock.try_lock().map_err(|_| ApplicationError::Conflict)?;
        let mut connection = Connection::open(path).map_err(storage)?;
        connection
            .busy_timeout(std::time::Duration::from_secs(5))
            .map_err(storage)?;
        connection
            .execute_batch("PRAGMA foreign_keys=ON;")
            .map_err(storage)?;
        let tx = connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(storage)?;
        let marker:bool=tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='gateway_format')",[],|row|row.get(0),
        ).map_err(storage)?;
        if marker {
            let (version, channel,source,expected): (i64, String,String,String) = tx
                .query_row(
                    "SELECT version,channel,source_digest,manifest FROM gateway_format WHERE singleton=1",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?,row.get(2)?,row.get(3)?)),
                )
                .map_err(storage)?;
            if !(1..=FORMAT).contains(&version)
                || channel != "enterprise-preview"
                || source != source_digest(version)
                || expected != manifest(&tx)?
            {
                return Err(ApplicationError::Invalid(
                    "unsupported execution ledger; preserve it for inspection".to_owned(),
                ));
            }
            if version < FORMAT {
                if version < 2 {
                    tx.execute_batch(DELIVERY_SCHEMA).map_err(storage)?;
                }
                if version < 3 {
                    tx.execute_batch(FORK_SCHEMA).map_err(storage)?;
                }
                tx.execute_batch(MERGE_SCHEMA).map_err(storage)?;
                tx.execute(
                    "UPDATE gateway_format SET version=?1,source_digest=?2,manifest=?3 WHERE singleton=1 AND version=?4",
                    params![FORMAT,source_digest(FORMAT),manifest(&tx)?,version],
                ).map_err(storage)?;
            }
        } else {
            let count: i64 = tx
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE name NOT LIKE 'sqlite_%'",
                    [],
                    |row| row.get(0),
                )
                .map_err(storage)?;
            if count != 0 {
                return Err(ApplicationError::Invalid(
                    "unmarked execution ledger".to_owned(),
                ));
            }
            tx.execute_batch(SCHEMA).map_err(storage)?;
            tx.execute_batch(DELIVERY_SCHEMA).map_err(storage)?;
            tx.execute_batch(FORK_SCHEMA).map_err(storage)?;
            tx.execute_batch(MERGE_SCHEMA).map_err(storage)?;
            tx.execute(
                "INSERT INTO gateway_format VALUES(1,?1,'enterprise-preview',?2,?3)",
                params![FORMAT, source_digest(FORMAT), manifest(&tx)?],
            )
            .map_err(storage)?;
        }
        tx.commit().map_err(storage)?;
        connection
            .execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;")
            .map_err(storage)?;
        Ok(Self {
            connection: Mutex::new(connection),
            _instance: lock,
        })
    }

    pub(crate) fn environment(
        &self,
        owner: &PrincipalKey,
        id: &EnvironmentId,
    ) -> Result<Environment, ApplicationError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| ApplicationError::Unavailable)?;
        environment(&connection, owner, id)
    }

    pub(crate) fn contains_environment(
        &self,
        owner: &PrincipalKey,
        id: &EnvironmentId,
    ) -> Result<bool, ApplicationError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| ApplicationError::Unavailable)?;
        connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM environment WHERE tenant=?1 AND principal=?2 AND id=?3)",
            params![owner.tenant_id.as_str(),owner.principal_id.as_str(),id.as_str()],|row|row.get(0),
        ).map_err(storage)
    }

    pub(crate) fn create_environment(
        &self,
        owner: &PrincipalKey,
        spec: &EnvironmentSpec,
    ) -> Result<Environment, ApplicationError> {
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| ApplicationError::Unavailable)?;
        let tx = connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(storage)?;
        let reserved:bool=tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM environment_fork WHERE tenant=?1 AND principal=?2 AND environment_id=?3)",
            params![owner.tenant_id.as_str(),owner.principal_id.as_str(),spec.id.as_str()],|row|row.get(0),
        ).map_err(storage)?;
        if reserved {
            return Err(ApplicationError::Conflict);
        }
        let value = serde_json::to_string(spec).map_err(storage)?;
        tx.execute(
            "INSERT INTO environment(tenant,principal,id,spec,revision) VALUES(?1,?2,?3,?4,1)
             ON CONFLICT(tenant,principal,id) DO NOTHING",
            params![
                owner.tenant_id.as_str(),
                owner.principal_id.as_str(),
                spec.id.as_str(),
                value
            ],
        )
        .map_err(storage)?;
        let existing = environment(&tx, owner, &spec.id)?;
        if existing.spec != *spec {
            return Err(ApplicationError::Conflict);
        }
        tx.commit().map_err(storage)?;
        Ok(existing)
    }

    pub(crate) fn require_idle(
        &self,
        owner: &PrincipalKey,
        id: &EnvironmentId,
        revision: u64,
    ) -> Result<Environment, ApplicationError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| ApplicationError::Unavailable)?;
        let environment = environment(&connection, owner, id)?;
        let active:Option<String>=connection.query_row(
            "SELECT active_operation FROM environment WHERE tenant=?1 AND principal=?2 AND id=?3",
            params![owner.tenant_id.as_str(),owner.principal_id.as_str(),id.as_str()],|row|row.get(0),
        ).map_err(storage)?;
        if active.is_some() || environment.revision != revision {
            return Err(ApplicationError::Conflict);
        }
        Ok(environment)
    }

    pub(crate) fn put_snapshot(
        &self,
        owner: &PrincipalKey,
        snapshot: &EnvironmentSnapshot,
    ) -> Result<(), ApplicationError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| ApplicationError::Unavailable)?;
        connection
            .execute(
                "INSERT INTO snapshot(tenant,principal,id,data) VALUES(?1,?2,?3,?4)",
                params![
                    owner.tenant_id.as_str(),
                    owner.principal_id.as_str(),
                    snapshot.id.as_str(),
                    serde_json::to_string(snapshot).map_err(storage)?
                ],
            )
            .map_err(storage)?;
        Ok(())
    }

    pub(crate) fn snapshot(
        &self,
        owner: &PrincipalKey,
        id: &EnvironmentSnapshotId,
    ) -> Result<EnvironmentSnapshot, ApplicationError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| ApplicationError::Unavailable)?;
        let raw: Option<String> = connection
            .query_row(
                "SELECT data FROM snapshot WHERE tenant=?1 AND principal=?2 AND id=?3",
                params![
                    owner.tenant_id.as_str(),
                    owner.principal_id.as_str(),
                    id.as_str()
                ],
                |row| row.get(0),
            )
            .optional()
            .map_err(storage)?;
        let snapshot: EnvironmentSnapshot =
            serde_json::from_str(&raw.ok_or(ApplicationError::NotFound)?).map_err(storage)?;
        if snapshot.id != *id {
            return Err(ApplicationError::Conflict);
        }
        Ok(snapshot)
    }

    pub(crate) fn release(
        &self,
        owner: &PrincipalKey,
        id: &EnvironmentId,
        revision: u64,
        finished: bool,
    ) -> Result<Option<Vec<OperationId>>, ApplicationError> {
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| ApplicationError::Unavailable)?;
        let tx = connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(storage)?;
        let row:Option<(i64,Option<String>,String)>=tx.query_row(
            "SELECT revision,active_operation,state FROM environment WHERE tenant=?1 AND principal=?2 AND id=?3",
            params![owner.tenant_id.as_str(),owner.principal_id.as_str(),id.as_str()],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?)),
        ).optional().map_err(storage)?;
        let (stored, active, state) = row.ok_or(ApplicationError::NotFound)?;
        if active.is_some() || u64::try_from(stored).map_err(storage)? != revision {
            return Err(ApplicationError::Conflict);
        }
        let pending: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM operation_delivery d JOIN operation o
             ON o.tenant=d.tenant AND o.principal=d.principal AND o.id=d.operation_id
             WHERE o.tenant=?1 AND o.principal=?2 AND o.environment_id=?3 AND d.acknowledged=0)",
                params![
                    owner.tenant_id.as_str(),
                    owner.principal_id.as_str(),
                    id.as_str()
                ],
                |row| row.get(0),
            )
            .map_err(storage)?;
        if pending {
            return Err(ApplicationError::Conflict);
        }
        let merge_pending:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM workspace_merge WHERE tenant=?1 AND principal=?2 AND environment_id=?3 AND acknowledged=0)",
            params![owner.tenant_id.as_str(),owner.principal_id.as_str(),id.as_str()],|row|row.get(0)).map_err(storage)?;
        if merge_pending {
            return Err(ApplicationError::Conflict);
        }
        if state == "released" {
            return Ok(None);
        }
        tx.execute(
            "UPDATE environment SET state=?4 WHERE tenant=?1 AND principal=?2 AND id=?3",
            params![
                owner.tenant_id.as_str(),
                owner.principal_id.as_str(),
                id.as_str(),
                if finished { "released" } else { "releasing" }
            ],
        )
        .map_err(storage)?;
        let containers = {
            let mut query=tx.prepare("SELECT id FROM operation WHERE tenant=?1 AND principal=?2 AND environment_id=?3").map_err(storage)?;
            let rows = query
                .query_map(
                    params![
                        owner.tenant_id.as_str(),
                        owner.principal_id.as_str(),
                        id.as_str()
                    ],
                    |row| row.get::<_, String>(0),
                )
                .map_err(storage)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(storage)?;
            rows.into_iter()
                .map(|id| OperationId::new(id).map_err(storage))
                .collect::<Result<Vec<_>, _>>()?
        };
        tx.commit().map_err(storage)?;
        Ok(Some(containers))
    }

    pub(crate) fn operation(
        &self,
        owner: &PrincipalKey,
        id: &OperationId,
    ) -> Result<Operation, ApplicationError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| ApplicationError::Unavailable)?;
        operation(&connection, owner, id)
    }

    pub(crate) fn scan_deliveries(&self, limit: u32) -> Result<Vec<Operation>, ApplicationError> {
        if !(1..=128).contains(&limit) {
            return Err(ApplicationError::Invalid(
                "invalid delivery scan bound".to_owned(),
            ));
        }
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| ApplicationError::Unavailable)?;
        let tx = connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(storage)?;
        let keys = {
            let mut query=tx.prepare(
                "SELECT tenant,principal,operation_id FROM operation_delivery WHERE acknowledged=0
                 ORDER BY scan_order,tenant,principal,operation_id LIMIT ?1",
            ).map_err(storage)?;
            query
                .query_map([limit], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })
                .map_err(storage)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(storage)?
        };
        let mut next: i64 = tx
            .query_row(
                "SELECT coalesce(max(scan_order),0) FROM operation_delivery",
                [],
                |row| row.get(0),
            )
            .map_err(storage)?;
        let mut result = Vec::with_capacity(keys.len());
        for (tenant, principal, id) in keys {
            let owner = PrincipalKey {
                tenant_id: zuno_types::identity::TenantId::new(tenant).map_err(storage)?,
                principal_id: zuno_types::identity::PrincipalId::new(principal).map_err(storage)?,
            };
            let id = OperationId::new(id).map_err(storage)?;
            result.push(operation(&tx, &owner, &id)?);
            next = next.checked_add(1).ok_or(ApplicationError::Conflict)?;
            tx.execute("UPDATE operation_delivery SET scan_order=?4 WHERE tenant=?1 AND principal=?2 AND operation_id=?3",
                params![owner.tenant_id.as_str(),owner.principal_id.as_str(),id.as_str(),next]).map_err(storage)?;
        }
        tx.commit().map_err(storage)?;
        Ok(result)
    }

    pub(crate) fn completion(
        &self,
        owner: &PrincipalKey,
        id: &OperationId,
    ) -> Result<Option<OperationCompletion>, ApplicationError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| ApplicationError::Unavailable)?;
        let (raw,digest):(Option<String>,Option<String>)=connection.query_row(
            "SELECT completion,completion_digest FROM operation_delivery WHERE tenant=?1 AND principal=?2 AND operation_id=?3",
            params![owner.tenant_id.as_str(),owner.principal_id.as_str(),id.as_str()],|row|Ok((row.get(0)?,row.get(1)?)),
        ).map_err(storage)?;
        let result = raw
            .map(|raw| serde_json::from_str::<OperationCompletion>(&raw).map_err(storage))
            .transpose()?;
        if let Some(result) = &result {
            result.validate()?;
            if result.lease.owner != *owner
                || result.operation.id != *id
                || digest.as_deref()
                    != Some(zuno_orchestration::sha256_json(&serde_json::json!(result)).as_str())
            {
                return Err(ApplicationError::Conflict);
            }
        }
        Ok(result)
    }

    pub(crate) fn capture(&self, completion: &OperationCompletion) -> Result<(), ApplicationError> {
        completion.validate()?;
        let owner = &completion.lease.owner;
        let id = &completion.operation.id;
        let digest = zuno_orchestration::sha256_json(&serde_json::json!(completion));
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| ApplicationError::Unavailable)?;
        let tx = connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(storage)?;
        let original = operation(&tx, owner, id)?;
        if original.lease != completion.lease
            || original.request != completion.operation
            || original.receipt != completion.receipt
        {
            return Err(ApplicationError::Conflict);
        }
        let old:Option<String>=tx.query_row(
            "SELECT completion_digest FROM operation_delivery WHERE tenant=?1 AND principal=?2 AND operation_id=?3",
            params![owner.tenant_id.as_str(),owner.principal_id.as_str(),id.as_str()],|row|row.get(0),
        ).map_err(storage)?;
        if let Some(old) = old {
            if old != digest {
                return Err(ApplicationError::Conflict);
            }
        } else {
            tx.execute(
                "UPDATE operation_delivery SET completion=?4,completion_digest=?5 WHERE tenant=?1 AND principal=?2 AND operation_id=?3",
                params![owner.tenant_id.as_str(),owner.principal_id.as_str(),id.as_str(),serde_json::to_string(completion).map_err(storage)?,digest],
            ).map_err(storage)?;
        }
        tx.commit().map_err(storage)
    }

    pub(crate) fn acknowledge(
        &self,
        completion: &OperationCompletion,
    ) -> Result<(), ApplicationError> {
        completion.validate()?;
        let owner = &completion.lease.owner;
        let digest = zuno_orchestration::sha256_json(&serde_json::json!(completion));
        let connection = self
            .connection
            .lock()
            .map_err(|_| ApplicationError::Unavailable)?;
        let changed=connection.execute(
            "UPDATE operation_delivery SET acknowledged=1 WHERE tenant=?1 AND principal=?2 AND operation_id=?3 AND completion_digest=?4",
            params![owner.tenant_id.as_str(),owner.principal_id.as_str(),completion.operation.id.as_str(),digest],
        ).map_err(storage)?;
        if changed != 1 {
            return Err(ApplicationError::Conflict);
        }
        Ok(())
    }

    pub(crate) fn admit(
        &self,
        lease: &ExecutionLease,
        request: &CommandOperation,
        container: String,
    ) -> Result<Operation, ApplicationError> {
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| ApplicationError::Unavailable)?;
        let tx = connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(storage)?;
        let owner = &lease.owner;
        let merge_exists:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM workspace_merge WHERE tenant=?1 AND principal=?2 AND id=?3)",
            params![owner.tenant_id.as_str(),owner.principal_id.as_str(),request.id.as_str()],|row|row.get(0)).map_err(storage)?;
        if merge_exists {
            return Err(ApplicationError::Conflict);
        }
        let digest = zuno_orchestration::sha256_json(&serde_json::json!([
            lease.job_id,
            lease.session_id,
            request
        ]));
        let existing: Option<String> = tx
            .query_row(
                "SELECT request_digest FROM operation WHERE tenant=?1 AND principal=?2 AND id=?3",
                params![
                    owner.tenant_id.as_str(),
                    owner.principal_id.as_str(),
                    request.id.as_str()
                ],
                |row| row.get(0),
            )
            .optional()
            .map_err(storage)?;
        if let Some(existing) = existing {
            if existing != digest {
                return Err(ApplicationError::Conflict);
            }
            return operation(&tx, owner, &request.id);
        }
        let environment = environment(&tx, owner, &request.environment_id)?;
        if environment.spec.session_id != lease.session_id
            || environment.revision != request.expected_revision
        {
            return Err(ApplicationError::Conflict);
        }
        let changed=tx.execute(
            "UPDATE environment SET active_operation=?4 WHERE tenant=?1 AND principal=?2 AND id=?3 AND active_operation IS NULL AND revision=?5",
            params![owner.tenant_id.as_str(),owner.principal_id.as_str(),request.environment_id.as_str(),
                request.id.as_str(),i64::try_from(request.expected_revision).map_err(|_|ApplicationError::Conflict)?],
        ).map_err(storage)?;
        if changed != 1 {
            return Err(ApplicationError::Conflict);
        }
        let receipt = OperationReceipt {
            id: request.id.clone(),
            environment_id: request.environment_id.clone(),
            phase: OperationPhase::Prepared,
            exit_code: None,
            cancellation_requested: false,
        };
        let value = Operation {
            owner: owner.clone(),
            lease: lease.clone(),
            request: request.clone(),
            container,
            receipt,
        };
        tx.execute(
            "INSERT INTO operation(tenant,principal,id,environment_id,request_digest,data) VALUES(?1,?2,?3,?4,?5,?6)",
            params![owner.tenant_id.as_str(),owner.principal_id.as_str(),request.id.as_str(),request.environment_id.as_str(),
                digest,serde_json::to_string(&value).map_err(storage)?],
        ).map_err(storage)?;
        tx.execute(
            "INSERT INTO operation_delivery(tenant,principal,operation_id) VALUES(?1,?2,?3)",
            params![
                owner.tenant_id.as_str(),
                owner.principal_id.as_str(),
                request.id.as_str()
            ],
        )
        .map_err(storage)?;
        tx.commit().map_err(storage)?;
        Ok(value)
    }

    pub(crate) fn begin_start(
        &self,
        owner: &PrincipalKey,
        id: &OperationId,
    ) -> Result<bool, ApplicationError> {
        self.update(owner, id, |operation| {
            if operation.receipt.phase != OperationPhase::Prepared {
                return Ok(false);
            }
            operation.receipt.phase = OperationPhase::Starting;
            Ok(true)
        })
    }

    pub(crate) fn cancel_requested(
        &self,
        owner: &PrincipalKey,
        id: &OperationId,
    ) -> Result<OperationReceipt, ApplicationError> {
        self.update(owner, id, |operation| {
            if matches!(
                operation.receipt.phase,
                OperationPhase::Completed | OperationPhase::Cancelled
            ) {
                return Ok(operation.receipt.clone());
            }
            operation.receipt.cancellation_requested = true;
            if operation.receipt.phase == OperationPhase::Prepared {
                operation.receipt.phase = OperationPhase::Cancelled;
            }
            Ok(operation.receipt.clone())
        })
    }

    pub(crate) fn observed(
        &self,
        owner: &PrincipalKey,
        id: &OperationId,
        phase: OperationPhase,
        exit_code: Option<i64>,
    ) -> Result<OperationReceipt, ApplicationError> {
        self.update(owner, id, |operation| {
            if matches!(
                operation.receipt.phase,
                OperationPhase::Completed | OperationPhase::Cancelled
            ) {
                return Ok(operation.receipt.clone());
            }
            if phase == OperationPhase::Prepared
                && operation.receipt.phase != OperationPhase::Prepared
            {
                // A stale inspection of a never-started container cannot undo
                // the durable start admission or grant another start.
                return Ok(operation.receipt.clone());
            }
            if phase == OperationPhase::Starting {
                return Err(ApplicationError::Conflict);
            }
            operation.receipt.phase =
                if phase == OperationPhase::Completed && operation.receipt.cancellation_requested {
                    OperationPhase::Cancelled
                } else {
                    phase
                };
            operation.receipt.exit_code = exit_code;
            Ok(operation.receipt.clone())
        })
    }

    fn update<T>(
        &self,
        owner: &PrincipalKey,
        id: &OperationId,
        change: impl FnOnce(&mut Operation) -> Result<T, ApplicationError>,
    ) -> Result<T, ApplicationError> {
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| ApplicationError::Unavailable)?;
        let tx = connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(storage)?;
        let mut value = operation(&tx, owner, id)?;
        let was_terminal = matches!(
            value.receipt.phase,
            OperationPhase::Completed | OperationPhase::Cancelled
        );
        let result = change(&mut value)?;
        tx.execute(
            "UPDATE operation SET data=?4 WHERE tenant=?1 AND principal=?2 AND id=?3",
            params![
                owner.tenant_id.as_str(),
                owner.principal_id.as_str(),
                id.as_str(),
                serde_json::to_string(&value).map_err(storage)?
            ],
        )
        .map_err(storage)?;
        if !was_terminal
            && matches!(
                value.receipt.phase,
                OperationPhase::Completed | OperationPhase::Cancelled
            )
        {
            let changed=tx.execute(
                "UPDATE environment SET active_operation=NULL,revision=revision+1 WHERE tenant=?1 AND principal=?2 AND id=?3 AND active_operation=?4",
                params![owner.tenant_id.as_str(),owner.principal_id.as_str(),value.request.environment_id.as_str(),id.as_str()],
            ).map_err(storage)?;
            if changed != 1 {
                return Err(ApplicationError::Conflict);
            }
        }
        tx.commit().map_err(storage)?;
        Ok(result)
    }
}

fn environment(
    connection: &Connection,
    owner: &PrincipalKey,
    id: &EnvironmentId,
) -> Result<Environment, ApplicationError> {
    let row: Option<(String, i64,String)> = connection
        .query_row(
            "SELECT spec,revision,state FROM environment WHERE tenant=?1 AND principal=?2 AND id=?3",
            params![
                owner.tenant_id.as_str(),
                owner.principal_id.as_str(),
                id.as_str()
            ],
            |row| Ok((row.get(0)?, row.get(1)?,row.get(2)?)),
        )
        .optional()
        .map_err(storage)?;
    let (raw, revision, state) = row.ok_or(ApplicationError::NotFound)?;
    if state != "active" {
        return Err(ApplicationError::NotFound);
    }
    let revision = u64::try_from(revision).map_err(storage)?;
    let spec: EnvironmentSpec = serde_json::from_str(&raw).map_err(storage)?;
    spec.validate()?;
    if spec.id != *id || revision == 0 {
        return Err(ApplicationError::Conflict);
    }
    Ok(Environment {
        owner: owner.clone(),
        spec,
        revision,
    })
}
fn operation(
    connection: &Connection,
    owner: &PrincipalKey,
    id: &OperationId,
) -> Result<Operation, ApplicationError> {
    let raw: Option<(String, String)> = connection
        .query_row(
            "SELECT data,request_digest FROM operation WHERE tenant=?1 AND principal=?2 AND id=?3",
            params![
                owner.tenant_id.as_str(),
                owner.principal_id.as_str(),
                id.as_str()
            ],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(storage)?;
    let (raw, digest) = raw.ok_or(ApplicationError::NotFound)?;
    let value: Operation = serde_json::from_str(&raw).map_err(storage)?;
    value.request.validate()?;
    if value.owner != *owner
        || value.request.id != *id
        || value.receipt.id != *id
        || value.receipt.environment_id != value.request.environment_id
        || digest
            != zuno_orchestration::sha256_json(&serde_json::json!([
                value.lease.job_id,
                value.lease.session_id,
                value.request
            ]))
    {
        return Err(ApplicationError::Conflict);
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zuno_types::identity::{
        ExecutionAttemptId, InvocationId, JobId, PrincipalScope, SessionId, WorkerInstanceId,
    };

    pub(super) fn fixture() -> (tempfile::TempDir, Ledger, ExecutionLease, CommandOperation) {
        let directory = tempfile::tempdir().unwrap();
        let ledger = Ledger::open(&directory.path().join("gateway.sqlite")).unwrap();
        let owner = PrincipalScope::local().owner();
        let environment = EnvironmentSpec {
            id: EnvironmentId::new("environment").unwrap(),
            session_id: SessionId::new("session").unwrap(),
            image: format!("image@sha256:{}", "a".repeat(64)),
            memory_bytes: 128 * 1024 * 1024,
            pids_limit: 64,
            cpu_millis: 1000,
        };
        ledger.create_environment(&owner, &environment).unwrap();
        let lease = ExecutionLease {
            owner,
            job_id: JobId::new("job").unwrap(),
            session_id: environment.session_id,
            attempt_id: ExecutionAttemptId::new("attempt").unwrap(),
            worker: WorkerInstanceId::new("worker").unwrap(),
            epoch: 1,
            checkpoint_version: 0,
            expires_at_ms: i64::MAX,
        };
        let request = CommandOperation {
            id: OperationId::new("operation").unwrap(),
            invocation_id: InvocationId::new("invocation").unwrap(),
            environment_id: environment.id,
            expected_revision: 1,
            argv: vec!["inspect".to_owned()],
        };
        (directory, ledger, lease, request)
    }

    #[test]
    fn start_is_single_use_and_uncertainty_keeps_the_environment_held() {
        let (_directory, ledger, lease, request) = fixture();
        ledger
            .admit(&lease, &request, "container".to_owned())
            .unwrap();
        assert!(ledger.begin_start(&lease.owner, &request.id).unwrap());
        assert!(!ledger.begin_start(&lease.owner, &request.id).unwrap());
        let stale = ledger
            .observed(&lease.owner, &request.id, OperationPhase::Prepared, None)
            .unwrap();
        assert_eq!(stale.phase, OperationPhase::Starting);
        assert!(!ledger.begin_start(&lease.owner, &request.id).unwrap());
        ledger
            .observed(&lease.owner, &request.id, OperationPhase::Uncertain, None)
            .unwrap();
        let mut other = request.clone();
        other.id = OperationId::new("other").unwrap();
        assert!(
            ledger
                .admit(&lease, &other, "other-container".to_owned())
                .is_err()
        );
        // A real late observation may resolve uncertainty without authorizing a
        // second start or incrementing the environment revision twice.
        ledger
            .observed(
                &lease.owner,
                &request.id,
                OperationPhase::Completed,
                Some(0),
            )
            .unwrap();
        ledger
            .observed(
                &lease.owner,
                &request.id,
                OperationPhase::Completed,
                Some(0),
            )
            .unwrap();
        assert_eq!(
            ledger
                .environment(&lease.owner, &request.environment_id)
                .unwrap()
                .revision,
            2
        );
        assert!(!ledger.begin_start(&lease.owner, &request.id).unwrap());
    }

    #[test]
    fn cancellation_before_start_prevents_execution_and_releases_once() {
        let (_directory, ledger, lease, request) = fixture();
        ledger
            .admit(&lease, &request, "container".to_owned())
            .unwrap();
        assert_eq!(
            ledger
                .cancel_requested(&lease.owner, &request.id)
                .unwrap()
                .phase,
            OperationPhase::Cancelled
        );
        assert!(!ledger.begin_start(&lease.owner, &request.id).unwrap());
        ledger.cancel_requested(&lease.owner, &request.id).unwrap();
        assert_eq!(
            ledger
                .environment(&lease.owner, &request.environment_id)
                .unwrap()
                .revision,
            2
        );
    }

    #[test]
    fn modified_requests_and_schema_drift_fail_closed() {
        let (directory, ledger, lease, request) = fixture();
        ledger
            .admit(&lease, &request, "container".to_owned())
            .unwrap();
        let connection = ledger.connection.lock().unwrap();
        connection.execute(
            "UPDATE operation SET data=json_set(data,'$.request.argv[0]','different') WHERE id='operation'",[],
        ).unwrap();
        drop(connection);
        assert!(ledger.operation(&lease.owner, &request.id).is_err());
        drop(ledger);
        let path = directory.path().join("gateway.sqlite");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch("CREATE TABLE unexpected(value TEXT)")
            .unwrap();
        drop(connection);
        assert!(Ledger::open(&path).is_err());
        let connection = Connection::open(&path).unwrap();
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM operation", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
    }

    #[test]
    fn one_gateway_instance_owns_a_ledger_until_it_closes() {
        let (directory, ledger, _, _) = fixture();
        let path = directory.path().join("gateway.sqlite");
        assert!(matches!(
            Ledger::open(&path),
            Err(ApplicationError::Conflict)
        ));
        drop(ledger);
        Ledger::open(&path).unwrap();
    }

    #[test]
    fn release_cannot_discard_output_before_completion_is_durably_acknowledged() {
        let (_directory, ledger, lease, request) = fixture();
        ledger
            .admit(&lease, &request, "container".to_owned())
            .unwrap();
        ledger.begin_start(&lease.owner, &request.id).unwrap();
        ledger
            .observed(
                &lease.owner,
                &request.id,
                OperationPhase::Completed,
                Some(0),
            )
            .unwrap();
        assert!(
            matches!(
                ledger.release(&lease.owner, &request.environment_id, 2, false),
                Err(ApplicationError::Conflict),
            ),
            "the result owner must acknowledge completion before output containers can be removed"
        );
        let completion = OperationCompletion {
            lease: lease.clone(),
            operation: request.clone(),
            receipt: ledger.operation(&lease.owner, &request.id).unwrap().receipt,
            output: Vec::new(),
            output_truncated: false,
        };
        ledger.capture(&completion).unwrap();
        assert!(
            ledger
                .release(&lease.owner, &request.environment_id, 2, false)
                .is_err()
        );
        ledger.acknowledge(&completion).unwrap();
        ledger
            .release(&lease.owner, &request.environment_id, 2, false)
            .unwrap();
    }

    #[test]
    fn captured_completion_survives_restart_and_only_matching_acknowledgement_releases_it() {
        let (directory, ledger, lease, request) = fixture();
        ledger
            .admit(&lease, &request, "container".to_owned())
            .unwrap();
        ledger.begin_start(&lease.owner, &request.id).unwrap();
        let receipt = ledger
            .observed(
                &lease.owner,
                &request.id,
                OperationPhase::Completed,
                Some(0),
            )
            .unwrap();
        let completion = OperationCompletion {
            lease: lease.clone(),
            operation: request.clone(),
            receipt,
            output: vec![zuno_application::environment::OperationOutput {
                channel: zuno_application::environment::OutputChannel::Stdout,
                bytes: b"saved output".to_vec(),
            }],
            output_truncated: false,
        };
        ledger.capture(&completion).unwrap();
        drop(ledger);
        let ledger = Ledger::open(&directory.path().join("gateway.sqlite")).unwrap();
        assert_eq!(
            ledger.completion(&lease.owner, &request.id).unwrap(),
            Some(completion.clone())
        );
        assert_eq!(ledger.scan_deliveries(1).unwrap().len(), 1);
        let mut changed = completion.clone();
        changed.output[0].bytes = b"changed".to_vec();
        assert!(ledger.capture(&changed).is_err());
        assert!(ledger.acknowledge(&changed).is_err());
        ledger.acknowledge(&completion).unwrap();
        ledger.acknowledge(&completion).unwrap();
        assert!(ledger.scan_deliveries(1).unwrap().is_empty());
    }

    #[test]
    fn format_one_delivery_upgrade_preserves_rows_and_rolls_back_before_its_marker() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("legacy.sqlite");
        let connection = Connection::open(&path).unwrap();
        connection.execute_batch(SCHEMA).unwrap();
        let (_fixture, ledger, lease, request) = fixture();
        ledger
            .admit(&lease, &request, "container".to_owned())
            .unwrap();
        ledger.begin_start(&lease.owner, &request.id).unwrap();
        ledger
            .observed(
                &lease.owner,
                &request.id,
                OperationPhase::Completed,
                Some(0),
            )
            .unwrap();
        let operation = ledger.operation(&lease.owner, &request.id).unwrap();
        let environment = ledger
            .environment(&lease.owner, &request.environment_id)
            .unwrap();
        connection.execute(
            "INSERT INTO environment(tenant,principal,id,spec,revision,state) VALUES(?1,?2,?3,?4,?5,'active')",
            params![lease.owner.tenant_id.as_str(),lease.owner.principal_id.as_str(),request.environment_id.as_str(),
                serde_json::to_string(&environment.spec).unwrap(),i64::try_from(environment.revision).unwrap()],
        ).unwrap();
        let raw = serde_json::to_string(&operation).unwrap();
        connection.execute(
            "INSERT INTO operation(tenant,principal,id,environment_id,request_digest,data) VALUES(?1,?2,?3,?4,?5,?6)",
            params![lease.owner.tenant_id.as_str(),lease.owner.principal_id.as_str(),request.id.as_str(),request.environment_id.as_str(),
                zuno_orchestration::sha256_json(&serde_json::json!([lease.job_id,lease.session_id,request])),raw],
        ).unwrap();
        connection
            .execute(
                "INSERT INTO gateway_format VALUES(1,1,'enterprise-preview',?1,?2)",
                params![
                    "4ef57090d4688535586706bb476f6dddd394a795838d726b03f770901feccb4c",
                    manifest(&connection).unwrap()
                ],
            )
            .unwrap();
        // Keep the injected trigger in the expected old manifest so validation
        // succeeds and the actual marker write is what fails.
        connection
            .execute_batch(
                "CREATE TRIGGER reject_delivery_marker BEFORE UPDATE OF version ON gateway_format
             BEGIN SELECT RAISE(ABORT,'injected marker failure'); END;",
            )
            .unwrap();
        connection
            .execute(
                "UPDATE gateway_format SET manifest=?1",
                [manifest(&connection).unwrap()],
            )
            .unwrap();
        drop(connection);
        assert!(Ledger::open(&path).is_err());
        let connection = Connection::open(&path).unwrap();
        assert_eq!(
            connection
                .query_row("SELECT version FROM gateway_format", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert!(
            !connection
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='operation_delivery')",
                    [],
                    |row| row.get::<_, bool>(0),
                )
                .unwrap()
        );
        assert_eq!(
            connection
                .query_row("SELECT data FROM operation", [], |row| row
                    .get::<_, String>(0))
                .unwrap(),
            raw
        );
        connection
            .execute_batch("DROP TRIGGER reject_delivery_marker")
            .unwrap();
        connection
            .execute(
                "UPDATE gateway_format SET manifest=?1",
                [manifest(&connection).unwrap()],
            )
            .unwrap();
        drop(connection);
        let migrated = Ledger::open(&path).unwrap();
        assert_eq!(
            migrated
                .operation(&lease.owner, &request.id)
                .unwrap()
                .receipt,
            operation.receipt
        );
        assert_eq!(migrated.scan_deliveries(1).unwrap().len(), 1);
        assert!(
            migrated
                .release(&lease.owner, &request.environment_id, 2, false)
                .is_err()
        );
        let connection = migrated.connection.lock().unwrap();
        assert_eq!(
            connection
                .query_row("SELECT version FROM gateway_format", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            FORMAT
        );
        assert_eq!(
            connection
                .query_row("SELECT data FROM operation", [], |row| row
                    .get::<_, String>(0))
                .unwrap(),
            raw
        );
    }
}
