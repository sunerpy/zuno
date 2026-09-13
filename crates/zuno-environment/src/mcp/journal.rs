use crate::storage;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::Value;
use std::{path::Path, sync::Mutex};
use zuno_application::{ApplicationError, mcp::*};
use zuno_types::identity::{OperationId, PrincipalKey};

const SCHEMA: &str = include_str!("schema.sql");
pub(super) struct Journal {
    connection: Mutex<Connection>,
    _instance: std::fs::File,
}
pub(super) struct Record {
    pub admission: McpAdmission,
    pub receipt: McpReceipt,
}
impl Record {
    pub fn key(&self) -> String {
        key(&self.admission.lease.owner, &self.admission.operation.id)
    }
    pub fn completion(&self) -> McpCompletion {
        McpCompletion {
            admission: self.admission.clone(),
            receipt: self.receipt.clone(),
        }
    }
}
fn key(owner: &PrincipalKey, id: &OperationId) -> String {
    zuno_orchestration::sha256_json(&serde_json::json!([owner, id]))
}
fn state(state: McpOperationState) -> Result<String, ApplicationError> {
    serde_json::to_value(state)
        .map_err(storage)?
        .as_str()
        .map(str::to_owned)
        .ok_or(ApplicationError::Conflict)
}
fn manifest(connection: &Connection) -> Result<String, ApplicationError> {
    let mut statement = connection.prepare(
        "SELECT type,name,tbl_name,sql FROM sqlite_master WHERE name NOT LIKE 'sqlite_%' ORDER BY type,name"
    ).map_err(storage)?;
    let rows = statement
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
fn read(connection: &Connection, key: &str) -> Result<Option<Record>, ApplicationError> {
    let row: Option<(String, String, String, String)> = connection
        .query_row(
            "SELECT admission,admission_digest,receipt,state FROM mcp_operation WHERE key=?1",
            [key],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()
        .map_err(storage)?;
    let Some((admission, digest, receipt, recorded_state)) = row else {
        return Ok(None);
    };
    let admission: McpAdmission = serde_json::from_str(&admission).map_err(storage)?;
    let receipt: McpReceipt = serde_json::from_str(&receipt).map_err(storage)?;
    admission.validate()?;
    let record = Record { admission, receipt };
    if record.key() != key
        || record.admission.digest() != digest
        || record.receipt.id != record.admission.operation.id
        || record.receipt.request_digest != record.admission.operation.digest()
        || state(record.receipt.state)? != recorded_state
    {
        return Err(ApplicationError::Conflict);
    }
    if record.receipt.state.terminal() {
        record.completion().validate()?;
    }
    Ok(Some(record))
}
fn write(connection: &Connection, record: &Record) -> Result<(), ApplicationError> {
    connection
        .execute(
            "UPDATE mcp_operation SET receipt=?2,state=?3 WHERE key=?1",
            params![
                record.key(),
                serde_json::to_string(&record.receipt).map_err(storage)?,
                state(record.receipt.state)?
            ],
        )
        .map_err(storage)?;
    Ok(())
}
impl Journal {
    pub fn open(path: &Path) -> Result<Self, ApplicationError> {
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
        let tx = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        let marker: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='mcp_format')",
                [],
                |row| row.get(0),
            )
            .map_err(storage)?;
        if marker {
            let actual:(i64,String,String,String)=tx.query_row("SELECT version,channel,source_digest,manifest FROM mcp_format WHERE singleton=1",[],
                |row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?))).map_err(storage)?;
            if actual
                != (
                    1,
                    "enterprise-preview".to_owned(),
                    zuno_orchestration::sha256_text(SCHEMA),
                    manifest(&tx)?,
                )
            {
                return Err(ApplicationError::Invalid(
                    "unsupported MCP journal; preserve for inspection".to_owned(),
                ));
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
                return Err(ApplicationError::Invalid("unmarked MCP journal".to_owned()));
            }
            tx.execute_batch(SCHEMA).map_err(storage)?;
            tx.execute(
                "INSERT INTO mcp_format VALUES(1,1,'enterprise-preview',?1,?2)",
                params![zuno_orchestration::sha256_text(SCHEMA), manifest(&tx)?],
            )
            .map_err(storage)?;
        }
        let keys = {
            let mut statement = tx
                .prepare("SELECT key FROM mcp_operation WHERE state='running'")
                .map_err(storage)?;
            statement
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(storage)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(storage)?
        };
        for key in keys {
            recover(&tx, &key)?;
        }
        tx.commit().map_err(storage)?;
        Ok(Self {
            connection: Mutex::new(connection),
            _instance: lock,
        })
    }
    pub fn get(&self, owner: &PrincipalKey, id: &OperationId) -> Result<Record, ApplicationError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| ApplicationError::Unavailable)?;
        read(&connection, &key(owner, id))?.ok_or(ApplicationError::NotFound)
    }
    pub fn admit(
        &self,
        admission: &McpAdmission,
        cancel: bool,
    ) -> Result<McpReceipt, ApplicationError> {
        admission.validate()?;
        let key = key(&admission.lease.owner, &admission.operation.id);
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| ApplicationError::Unavailable)?;
        let tx = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        let mut record = if let Some(old) = read(&tx, &key)? {
            if old.admission.digest() != admission.digest() {
                return Err(ApplicationError::Conflict);
            }
            old
        } else {
            let receipt = McpReceipt {
                id: admission.operation.id.clone(),
                request_digest: admission.operation.digest(),
                state: McpOperationState::Queued,
                cancellation_requested: false,
                result: None,
                failure: None,
            };
            tx.execute("INSERT INTO mcp_operation(key,tenant,principal,operation_id,admission,admission_digest,receipt,state) VALUES(?1,?2,?3,?4,?5,?6,?7,'queued')",
                params![key,admission.lease.owner.tenant_id.as_str(),admission.lease.owner.principal_id.as_str(),admission.operation.id.as_str(),
                    serde_json::to_string(admission).map_err(storage)?,admission.digest(),serde_json::to_string(&receipt).map_err(storage)?]).map_err(storage)?;
            Record {
                admission: admission.clone(),
                receipt,
            }
        };
        if cancel && !record.receipt.state.terminal() {
            record.receipt.cancellation_requested = true;
            if record.receipt.state == McpOperationState::Queued {
                record.receipt.state = McpOperationState::Cancelled;
                record.receipt.failure = Some(McpFailure::CancelledBeforeCall);
            }
            write(&tx, &record)?;
        }
        tx.commit().map_err(storage)?;
        Ok(record.receipt)
    }
    pub fn start(&self, record: &Record) -> Result<bool, ApplicationError> {
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| ApplicationError::Unavailable)?;
        let tx = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        let mut current = read(&tx, &record.key())?.ok_or(ApplicationError::NotFound)?;
        if current.receipt.state != McpOperationState::Queued {
            return Ok(false);
        }
        current.receipt.state = McpOperationState::Running;
        write(&tx, &current)?;
        tx.commit().map_err(storage)?;
        Ok(true)
    }
    pub fn finish(
        &self,
        record: &Record,
        state: McpOperationState,
        result: Option<Value>,
        failure: Option<McpFailure>,
    ) -> Result<(), ApplicationError> {
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| ApplicationError::Unavailable)?;
        let tx = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        let mut current = read(&tx, &record.key())?.ok_or(ApplicationError::NotFound)?;
        if current.receipt.state.terminal() {
            return Ok(());
        }
        current.receipt.state = state;
        current.receipt.result = result;
        current.receipt.failure = failure;
        current.completion().validate()?;
        write(&tx, &current)?;
        tx.commit().map_err(storage)
    }
    pub fn recover_key(&self, key: &str) -> Result<(), ApplicationError> {
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| ApplicationError::Unavailable)?;
        let tx = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        recover(&tx, key)?;
        tx.commit().map_err(storage)
    }
    pub fn scan(&self, limit: u32) -> Result<Vec<Record>, ApplicationError> {
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| ApplicationError::Unavailable)?;
        let tx = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        let keys = {
            let mut statement=tx.prepare("SELECT key FROM mcp_operation WHERE acknowledged=0 AND retry_at<=CAST(unixepoch('subsec')*1000 AS INTEGER) ORDER BY scan_order,key LIMIT ?1").map_err(storage)?;
            statement
                .query_map([limit], |row| row.get::<_, String>(0))
                .map_err(storage)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(storage)?
        };
        let mut sequence: i64 = tx
            .query_row(
                "SELECT COALESCE(MAX(scan_order),0) FROM mcp_operation",
                [],
                |row| row.get(0),
            )
            .map_err(storage)?;
        let mut output = Vec::new();
        for key in keys {
            sequence = sequence.checked_add(1).ok_or(ApplicationError::Conflict)?;
            let record = read(&tx, &key)?.ok_or(ApplicationError::Conflict)?;
            tx.execute(
                "UPDATE mcp_operation SET scan_order=?2 WHERE key=?1",
                params![key, sequence],
            )
            .map_err(storage)?;
            output.push(record);
        }
        tx.commit().map_err(storage)?;
        Ok(output)
    }
    pub fn acknowledge(&self, record: &Record) -> Result<(), ApplicationError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| ApplicationError::Unavailable)?;
        let current = read(&connection, &record.key())?.ok_or(ApplicationError::NotFound)?;
        if current.receipt != record.receipt || !current.receipt.state.terminal() {
            return Err(ApplicationError::Conflict);
        }
        connection
            .execute(
                "UPDATE mcp_operation SET acknowledged=1 WHERE key=?1",
                [record.key()],
            )
            .map_err(storage)?;
        Ok(())
    }
    pub fn defer(&self, key: &str) -> Result<(), ApplicationError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| ApplicationError::Unavailable)?;
        connection.execute("UPDATE mcp_operation SET retry_count=MIN(retry_count+1,6),retry_at=CAST(unixepoch('subsec')*1000 AS INTEGER)+MIN(30000,500*(1<<MIN(retry_count,6))) WHERE key=?1",[key]).map_err(storage)?;
        Ok(())
    }
}
fn recover(connection: &Connection, key: &str) -> Result<(), ApplicationError> {
    if let Some(mut record) = read(connection, key)?
        && record.receipt.state == McpOperationState::Running
    {
        record.receipt.state = McpOperationState::Uncertain;
        record.receipt.failure = Some(McpFailure::LostOutcome);
        record.completion().validate()?;
        write(connection, &record)?;
    }
    Ok(())
}
