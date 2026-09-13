use super::*;
use zuno_application::workspace_edit::{
    WorkspaceEditAdmission, WorkspaceEditCompletion, WorkspaceEditReceipt, WorkspaceEditState,
};

#[derive(Clone)]
pub(crate) struct EditRecord {
    pub admission: WorkspaceEditAdmission,
    pub volume: String,
    pub nonce: String,
    pub state: WorkspaceEditState,
    pub receipt: Option<WorkspaceEditReceipt>,
}
fn digest(admission: &WorkspaceEditAdmission) -> String {
    zuno_orchestration::sha256_json(&serde_json::json!([
        admission.gateway_id,
        admission.lease.owner,
        admission.lease.job_id,
        admission.lease.session_id,
        admission.environment,
        admission.operation,
        admission.base,
        admission.review
    ]))
}
fn volume(admission: &WorkspaceEditAdmission) -> String {
    format!(
        "zuno-edit-{}",
        zuno_orchestration::sha256_json(&serde_json::json!([
            admission.lease.owner,
            admission.operation.environment_id,
            admission.operation.id
        ]))
    )
}
fn read(
    connection: &Connection,
    owner: &PrincipalKey,
    id: &OperationId,
) -> Result<Option<EditRecord>, ApplicationError> {
    let row:Option<(String,String,String,String,String,Option<String>)>=connection.query_row(
        "SELECT request_digest,admission,volume,nonce,state,receipt FROM workspace_edit WHERE tenant=?1 AND principal=?2 AND id=?3",
        params![owner.tenant_id.as_str(),owner.principal_id.as_str(),id.as_str()],
        |row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?))
    ).optional().map_err(storage)?;
    let Some((hash, value, physical, nonce, state, receipt)) = row else {
        return Ok(None);
    };
    let admission: WorkspaceEditAdmission = serde_json::from_str(&value).map_err(storage)?;
    admission.validate()?;
    let state: WorkspaceEditState =
        serde_json::from_value(serde_json::Value::String(state)).map_err(storage)?;
    let receipt: Option<WorkspaceEditReceipt> = receipt
        .map(|value| serde_json::from_str(&value))
        .transpose()
        .map_err(storage)?;
    if admission.lease.owner != *owner
        || admission.operation.id != *id
        || digest(&admission) != hash
        || volume(&admission) != physical
        || nonce.len() != 32
        || !nonce.bytes().all(|byte| byte.is_ascii_hexdigit())
        || (state == WorkspaceEditState::Committed) != receipt.is_some()
    {
        return Err(ApplicationError::Conflict);
    }
    if let Some(receipt) = &receipt {
        WorkspaceEditCompletion {
            admission: admission.clone(),
            receipt: receipt.clone(),
        }
        .validate()?;
    }
    Ok(Some(EditRecord {
        admission,
        volume: physical,
        nonce,
        state,
        receipt,
    }))
}

impl Ledger {
    pub(crate) fn cancel_admitted_edit(
        &self,
        admission: &WorkspaceEditAdmission,
    ) -> Result<EditRecord, ApplicationError> {
        admission.validate()?;
        let owner = &admission.lease.owner;
        let operation = &admission.operation;
        {
            let mut connection = self
                .connection
                .lock()
                .map_err(|_| ApplicationError::Unavailable)?;
            let tx = connection
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .map_err(storage)?;
            if let Some(record) = read(&tx, owner, &operation.id)? {
                if digest(&record.admission) != digest(admission) {
                    return Err(ApplicationError::Conflict);
                }
            } else {
                let collision:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM operation WHERE tenant=?1 AND principal=?2 AND id=?3)
                    OR EXISTS(SELECT 1 FROM workspace_merge WHERE tenant=?1 AND principal=?2 AND id=?3)",
                    params![owner.tenant_id.as_str(),owner.principal_id.as_str(),operation.id.as_str()],|row|row.get(0)).map_err(storage)?;
                if collision
                    || environment(&tx, owner, &operation.environment_id)?
                        .spec
                        .session_id
                        != admission.lease.session_id
                {
                    return Err(ApplicationError::Conflict);
                }
                tx.execute("INSERT INTO workspace_edit(tenant,principal,id,environment_id,request_digest,admission,volume,nonce,state)
                    VALUES(?1,?2,?3,?4,?5,?6,?7,?8,'cancelled')",
                    params![owner.tenant_id.as_str(),owner.principal_id.as_str(),operation.id.as_str(),operation.environment_id.as_str(),
                        digest(admission),serde_json::to_string(admission).map_err(storage)?,volume(admission),uuid::Uuid::new_v4().simple().to_string()]).map_err(storage)?;
            }
            tx.commit().map_err(storage)?;
        }
        self.cancel_edit(owner, &operation.id)
    }
    pub(crate) fn scan_edits(&self, limit: u32) -> Result<Vec<EditRecord>, ApplicationError> {
        if !(1..=32).contains(&limit) {
            return Err(ApplicationError::Invalid(
                "invalid edit scan bound".to_owned(),
            ));
        }
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| ApplicationError::Unavailable)?;
        let tx = connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(storage)?;
        let rows = {
            let mut statement = tx
                .prepare(
                    "SELECT tenant,principal,id FROM workspace_edit
                WHERE acknowledged=0 AND state IN('preparing','committed','cancelled')
                  AND retry_at<=CAST(unixepoch('subsec')*1000 AS INTEGER)
                ORDER BY scan_order,tenant,principal,id LIMIT ?1",
                )
                .map_err(storage)?;
            statement
                .query_map(params![limit], |row| {
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
        let mut sequence: i64 = tx
            .query_row(
                "SELECT COALESCE(MAX(scan_order),0) FROM workspace_edit",
                [],
                |row| row.get(0),
            )
            .map_err(storage)?;
        let mut records = Vec::new();
        for (tenant, principal, id) in rows {
            let owner = PrincipalKey {
                tenant_id: zuno_types::identity::TenantId::new(tenant).map_err(storage)?,
                principal_id: zuno_types::identity::PrincipalId::new(principal).map_err(storage)?,
            };
            let id = OperationId::new(id).map_err(storage)?;
            records.push(read(&tx, &owner, &id)?.ok_or(ApplicationError::Conflict)?);
            sequence = sequence.checked_add(1).ok_or(ApplicationError::Conflict)?;
            tx.execute("UPDATE workspace_edit SET scan_order=?4 WHERE tenant=?1 AND principal=?2 AND id=?3",
                params![owner.tenant_id.as_str(),owner.principal_id.as_str(),id.as_str(),sequence]).map_err(storage)?;
        }
        tx.commit().map_err(storage)?;
        Ok(records)
    }
    pub(crate) fn defer_edit(
        &self,
        owner: &PrincipalKey,
        id: &OperationId,
    ) -> Result<(), ApplicationError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| ApplicationError::Unavailable)?;
        connection.execute("UPDATE workspace_edit SET retry_count=retry_count+1,
            retry_at=CAST(unixepoch('subsec')*1000 AS INTEGER)+MIN(30000,1000*(1<<MIN(retry_count,5)))
            WHERE tenant=?1 AND principal=?2 AND id=?3 AND acknowledged=0",
            params![owner.tenant_id.as_str(),owner.principal_id.as_str(),id.as_str()]).map_err(storage)?;
        Ok(())
    }
    pub(crate) fn edit_completion(
        &self,
        owner: &PrincipalKey,
        id: &OperationId,
    ) -> Result<Option<WorkspaceEditCompletion>, ApplicationError> {
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| ApplicationError::Unavailable)?;
        let tx = connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(storage)?;
        let record = read(&tx, owner, id)?.ok_or(ApplicationError::NotFound)?;
        let receipt = match record.state {
            WorkspaceEditState::Committed => record.receipt.ok_or(ApplicationError::Conflict)?,
            WorkspaceEditState::Cancelled => WorkspaceEditReceipt {
                id: id.clone(),
                environment_id: record.admission.operation.environment_id.clone(),
                state: WorkspaceEditState::Cancelled,
                request_digest: record.admission.operation.digest(),
                revision: record.admission.operation.expected_revision,
            },
            _ => return Ok(None),
        };
        let completion = WorkspaceEditCompletion {
            admission: record.admission,
            receipt,
        };
        completion.validate()?;
        let hash = zuno_orchestration::sha256_json(&serde_json::json!(completion));
        let prior:Option<String>=tx.query_row("SELECT completion_digest FROM workspace_edit WHERE tenant=?1 AND principal=?2 AND id=?3",
            params![owner.tenant_id.as_str(),owner.principal_id.as_str(),id.as_str()],|row|row.get(0)).map_err(storage)?;
        if prior.as_ref().is_some_and(|value| value != &hash) {
            return Err(ApplicationError::Conflict);
        }
        tx.execute("UPDATE workspace_edit SET completion=?4,completion_digest=?5 WHERE tenant=?1 AND principal=?2 AND id=?3 AND completion IS NULL",
            params![owner.tenant_id.as_str(),owner.principal_id.as_str(),id.as_str(),serde_json::to_string(&completion).map_err(storage)?,hash]).map_err(storage)?;
        tx.commit().map_err(storage)?;
        Ok(Some(completion))
    }
    pub(crate) fn acknowledge_edit(
        &self,
        completion: &WorkspaceEditCompletion,
    ) -> Result<(), ApplicationError> {
        completion.validate()?;
        let owner = &completion.admission.lease.owner;
        let connection = self
            .connection
            .lock()
            .map_err(|_| ApplicationError::Unavailable)?;
        let changed=connection.execute("UPDATE workspace_edit SET acknowledged=1 WHERE tenant=?1 AND principal=?2 AND id=?3 AND completion_digest=?4",
            params![owner.tenant_id.as_str(),owner.principal_id.as_str(),completion.receipt.id.as_str(),
                zuno_orchestration::sha256_json(&serde_json::json!(completion))]).map_err(storage)?;
        if changed != 1 {
            return Err(ApplicationError::Conflict);
        }
        Ok(())
    }
    pub(crate) fn edit(
        &self,
        owner: &PrincipalKey,
        id: &OperationId,
    ) -> Result<EditRecord, ApplicationError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| ApplicationError::Unavailable)?;
        read(&connection, owner, id)?.ok_or(ApplicationError::NotFound)
    }
    pub(crate) fn active_edit(
        &self,
        owner: &PrincipalKey,
        id: &EnvironmentId,
    ) -> Result<Option<EditRecord>, ApplicationError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| ApplicationError::Unavailable)?;
        let selected:Option<String>=connection.query_row(
            "SELECT edit_id FROM environment_volume WHERE tenant=?1 AND principal=?2 AND environment_id=?3 AND edit_id IS NOT NULL",
            params![owner.tenant_id.as_str(),owner.principal_id.as_str(),id.as_str()],|row|row.get(0),
        ).optional().map_err(storage)?;
        selected
            .map(|id| {
                let record = read(&connection, owner, &OperationId::new(id).map_err(storage)?)?
                    .ok_or(ApplicationError::Conflict)?;
                if record.state != WorkspaceEditState::Committed {
                    return Err(ApplicationError::Conflict);
                }
                Ok(record)
            })
            .transpose()
    }
    pub(crate) fn begin_edit(
        &self,
        admission: &WorkspaceEditAdmission,
    ) -> Result<EditRecord, ApplicationError> {
        admission.validate()?;
        let owner = &admission.lease.owner;
        let operation = &admission.operation;
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| ApplicationError::Unavailable)?;
        let tx = connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(storage)?;
        if let Some(record) = read(&tx, owner, &operation.id)? {
            if digest(&record.admission) != digest(admission) {
                return Err(ApplicationError::Conflict);
            }
            return Ok(record);
        }
        let collision: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM operation WHERE tenant=?1 AND principal=?2 AND id=?3)
            OR EXISTS(SELECT 1 FROM workspace_merge WHERE tenant=?1 AND principal=?2 AND id=?3)",
                params![
                    owner.tenant_id.as_str(),
                    owner.principal_id.as_str(),
                    operation.id.as_str()
                ],
                |row| row.get(0),
            )
            .map_err(storage)?;
        if collision {
            return Err(ApplicationError::Conflict);
        }
        if environment(&tx, owner, &operation.environment_id)? != admission.environment {
            return Err(ApplicationError::Conflict);
        }
        let changed=tx.execute("UPDATE environment SET active_operation=?4
            WHERE tenant=?1 AND principal=?2 AND id=?3 AND revision=?5 AND active_operation IS NULL AND state='active'",
            params![owner.tenant_id.as_str(),owner.principal_id.as_str(),operation.environment_id.as_str(),operation.id.as_str(),i64::try_from(operation.expected_revision).map_err(storage)?]).map_err(storage)?;
        if changed != 1 {
            return Err(ApplicationError::Conflict);
        }
        tx.execute("INSERT INTO workspace_edit(tenant,principal,id,environment_id,request_digest,admission,volume,nonce,state)
            VALUES(?1,?2,?3,?4,?5,?6,?7,?8,'preparing')",
            params![owner.tenant_id.as_str(),owner.principal_id.as_str(),operation.id.as_str(),operation.environment_id.as_str(),
                digest(admission),serde_json::to_string(admission).map_err(storage)?,volume(admission),uuid::Uuid::new_v4().simple().to_string()]).map_err(storage)?;
        let record = read(&tx, owner, &operation.id)?.ok_or(ApplicationError::Conflict)?;
        tx.commit().map_err(storage)?;
        Ok(record)
    }
    pub(crate) fn publish_edit(
        &self,
        owner: &PrincipalKey,
        id: &OperationId,
    ) -> Result<WorkspaceEditReceipt, ApplicationError> {
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| ApplicationError::Unavailable)?;
        let tx = connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(storage)?;
        let record = read(&tx, owner, id)?.ok_or(ApplicationError::NotFound)?;
        if let Some(receipt) = record.receipt {
            return Ok(receipt);
        }
        if record.state != WorkspaceEditState::Preparing {
            return Err(ApplicationError::Conflict);
        }
        let request = &record.admission.operation;
        let revision = request
            .expected_revision
            .checked_add(1)
            .ok_or(ApplicationError::Conflict)?;
        let changed=tx.execute("UPDATE environment SET active_operation=NULL,revision=?5
            WHERE tenant=?1 AND principal=?2 AND id=?3 AND active_operation=?4 AND revision=?6 AND state='active'",
            params![owner.tenant_id.as_str(),owner.principal_id.as_str(),request.environment_id.as_str(),id.as_str(),i64::try_from(revision).map_err(storage)?,i64::try_from(request.expected_revision).map_err(storage)?]).map_err(storage)?;
        if changed != 1 {
            return Err(ApplicationError::Conflict);
        }
        let receipt = WorkspaceEditReceipt {
            id: id.clone(),
            environment_id: request.environment_id.clone(),
            state: WorkspaceEditState::Committed,
            request_digest: request.digest(),
            revision,
        };
        tx.execute("UPDATE workspace_edit SET state='committed',receipt=?4 WHERE tenant=?1 AND principal=?2 AND id=?3",
            params![owner.tenant_id.as_str(),owner.principal_id.as_str(),id.as_str(),serde_json::to_string(&receipt).map_err(storage)?]).map_err(storage)?;
        tx.execute("INSERT INTO environment_volume(tenant,principal,environment_id,edit_id) VALUES(?1,?2,?3,?4)
            ON CONFLICT(tenant,principal,environment_id) DO UPDATE SET edit_id=excluded.edit_id,merge_id=NULL",
            params![owner.tenant_id.as_str(),owner.principal_id.as_str(),request.environment_id.as_str(),id.as_str()]).map_err(storage)?;
        tx.commit().map_err(storage)?;
        Ok(receipt)
    }
    pub(crate) fn cancel_edit(
        &self,
        owner: &PrincipalKey,
        id: &OperationId,
    ) -> Result<EditRecord, ApplicationError> {
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| ApplicationError::Unavailable)?;
        let tx = connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(storage)?;
        let record = read(&tx, owner, id)?.ok_or(ApplicationError::NotFound)?;
        if record.state != WorkspaceEditState::Preparing {
            return Ok(record);
        }
        let request = &record.admission.operation;
        let changed = tx
            .execute(
                "UPDATE environment SET active_operation=NULL
            WHERE tenant=?1 AND principal=?2 AND id=?3 AND active_operation=?4 AND revision=?5",
                params![
                    owner.tenant_id.as_str(),
                    owner.principal_id.as_str(),
                    request.environment_id.as_str(),
                    id.as_str(),
                    i64::try_from(request.expected_revision).map_err(storage)?
                ],
            )
            .map_err(storage)?;
        if changed != 1 {
            return Err(ApplicationError::Conflict);
        }
        tx.execute("UPDATE workspace_edit SET state='cancelled' WHERE tenant=?1 AND principal=?2 AND id=?3",
            params![owner.tenant_id.as_str(),owner.principal_id.as_str(),id.as_str()]).map_err(storage)?;
        let record = read(&tx, owner, id)?.ok_or(ApplicationError::Conflict)?;
        tx.commit().map_err(storage)?;
        Ok(record)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zuno_application::workspace_edit::{
        FileExpectation, WorkspaceEditOperation, WorkspaceEditReview, WorkspaceFileEdit,
    };
    fn admission(
        lease: &ExecutionLease,
        command: &CommandOperation,
        environment: Environment,
    ) -> WorkspaceEditAdmission {
        let operation = WorkspaceEditOperation {
            id: OperationId::new("edit-fixture").unwrap(),
            invocation_id: command.invocation_id.clone(),
            environment_id: command.environment_id.clone(),
            expected_revision: environment.revision,
            edits: vec![WorkspaceFileEdit {
                path: zuno_application::workspace_merge::WorkspacePath::new("new").unwrap(),
                expected: FileExpectation::Absent,
                content: Some("created".to_owned()),
            }],
        };
        WorkspaceEditAdmission {
            gateway_id: zuno_types::identity::GatewayId::new("gateway").unwrap(),
            lease: lease.clone(),
            base: EnvironmentSnapshot {
                id: EnvironmentSnapshotId::new("base").unwrap(),
                environment_id: environment.spec.id.clone(),
                revision: environment.revision,
                sha256: "a".repeat(64),
                bytes: 1024,
            },
            environment,
            operation,
            review: vec![WorkspaceEditReview {
                path: zuno_application::workspace_merge::WorkspacePath::new("new").unwrap(),
                before: None,
                after: Some("created".to_owned()),
            }],
        }
    }
    #[test]
    fn publication_pointer_receipt_and_revision_commit_or_roll_back_together() {
        let (_directory, ledger, lease, command) = super::super::tests::fixture();
        let environment = ledger
            .environment(&lease.owner, &command.environment_id)
            .unwrap();
        let admission = admission(&lease, &command, environment.clone());
        ledger.begin_edit(&admission).unwrap();
        ledger.connection.lock().unwrap().execute_batch(
            "CREATE TRIGGER refuse_edit BEFORE INSERT ON environment_volume BEGIN SELECT RAISE(ABORT,'injected edit publication'); END;"
        ).unwrap();
        assert!(
            ledger
                .publish_edit(&lease.owner, &admission.operation.id)
                .is_err()
        );
        assert_eq!(
            ledger
                .environment(&lease.owner, &command.environment_id)
                .unwrap()
                .revision,
            environment.revision
        );
        assert!(
            ledger
                .active_edit(&lease.owner, &command.environment_id)
                .unwrap()
                .is_none()
        );
        ledger
            .connection
            .lock()
            .unwrap()
            .execute_batch("DROP TRIGGER refuse_edit")
            .unwrap();
        let receipt = ledger
            .publish_edit(&lease.owner, &admission.operation.id)
            .unwrap();
        assert_eq!(receipt.revision, environment.revision + 1);
        assert_eq!(
            ledger
                .publish_edit(&lease.owner, &admission.operation.id)
                .unwrap(),
            receipt
        );
        assert!(
            ledger
                .admit(
                    &lease,
                    &CommandOperation {
                        id: admission.operation.id.clone(),
                        ..command.clone()
                    },
                    "other".to_owned()
                )
                .is_err()
        );
    }
    #[test]
    fn exact_format_four_migration_preserves_rows_and_restores_original_on_failure() {
        let (directory, ledger, lease, command) = super::super::tests::fixture();
        let environment = ledger
            .environment(&lease.owner, &command.environment_id)
            .unwrap();
        drop(ledger);
        let path = directory.path().join("format-four.sqlite");
        let connection = Connection::open(&path).unwrap();
        connection.execute_batch(SCHEMA).unwrap();
        connection.execute_batch(DELIVERY_SCHEMA).unwrap();
        connection.execute_batch(FORK_SCHEMA).unwrap();
        connection
            .execute_batch(include_str!("../fixtures/format4-merge.sql"))
            .unwrap();
        connection
            .execute(
                "INSERT INTO environment(tenant,principal,id,spec,revision) VALUES(?1,?2,?3,?4,1)",
                params![
                    lease.owner.tenant_id.as_str(),
                    lease.owner.principal_id.as_str(),
                    environment.spec.id.as_str(),
                    serde_json::to_string(&environment.spec).unwrap()
                ],
            )
            .unwrap();
        connection.execute_batch("CREATE TRIGGER refuse_marker BEFORE UPDATE OF version ON gateway_format BEGIN SELECT RAISE(ABORT,'injected marker failure'); END;").unwrap();
        connection
            .execute(
                "INSERT INTO gateway_format VALUES(1,4,'enterprise-preview',?1,?2)",
                params![
                    "54a732e7ef97d171824eb073d21a466353534c518b0c64e93b4ec2c227ac8b7e",
                    manifest(&connection).unwrap()
                ],
            )
            .unwrap();
        let original: String = connection
            .query_row("SELECT spec FROM environment", [], |row| row.get(0))
            .unwrap();
        drop(connection);
        assert!(Ledger::open(&path).is_err());
        let connection = Connection::open(&path).unwrap();
        assert_eq!(
            connection
                .query_row("SELECT version FROM gateway_format", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            4
        );
        assert_eq!(
            connection
                .query_row("SELECT spec FROM environment", [], |row| row
                    .get::<_, String>(0))
                .unwrap(),
            original
        );
        assert!(
            !connection
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='workspace_edit')",
                    [],
                    |row| row.get::<_, bool>(0)
                )
                .unwrap()
        );
        connection
            .execute_batch("DROP TRIGGER refuse_marker")
            .unwrap();
        connection
            .execute(
                "UPDATE gateway_format SET manifest=?1",
                params![manifest(&connection).unwrap()],
            )
            .unwrap();
        drop(connection);
        let migrated = Ledger::open(&path).unwrap();
        assert_eq!(
            migrated
                .environment(&lease.owner, &environment.spec.id)
                .unwrap(),
            environment
        );
        assert_eq!(
            migrated
                .connection
                .lock()
                .unwrap()
                .query_row("SELECT spec FROM environment", [], |row| row
                    .get::<_, String>(0))
                .unwrap(),
            original
        );
        assert_eq!(
            migrated
                .connection
                .lock()
                .unwrap()
                .query_row("SELECT version FROM gateway_format", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            FORMAT
        );
    }
}
