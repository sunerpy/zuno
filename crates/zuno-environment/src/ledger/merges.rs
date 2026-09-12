//! Copy-on-write workspace publication. Restoring a candidate never changes the
//! active volume; its pointer, revision and receipt publish in one transaction.
use super::*;
use zuno_application::workspace_merge::{
    WorkspaceMergeOperation, WorkspaceMergeReceipt, WorkspaceMergeState,
};

#[derive(Debug, Clone)]
pub(crate) struct MergeRecord {
    pub request: WorkspaceMergeOperation,
    pub lease: ExecutionLease,
    pub volume: String,
    pub nonce: String,
    pub state: WorkspaceMergeState,
    pub receipt: Option<WorkspaceMergeReceipt>,
}
fn volume(owner: &PrincipalKey, request: &WorkspaceMergeOperation) -> String {
    format!(
        "zuno-merge-{}",
        zuno_orchestration::sha256_json(&serde_json::json!([
            owner,
            request.environment_id,
            request.id
        ]))
    )
}
fn digest(lease: &ExecutionLease, request: &WorkspaceMergeOperation) -> String {
    zuno_orchestration::sha256_json(&serde_json::json!([
        lease.owner,
        lease.job_id,
        lease.session_id,
        request
    ]))
}
fn read(
    connection: &Connection,
    owner: &PrincipalKey,
    id: &OperationId,
) -> Result<Option<MergeRecord>, ApplicationError> {
    let row = connection
        .query_row(
            "SELECT request_digest,request,lease,volume,nonce,state,receipt FROM workspace_merge
        WHERE tenant=?1 AND principal=?2 AND id=?3",
            params![
                owner.tenant_id.as_str(),
                owner.principal_id.as_str(),
                id.as_str()
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, Option<String>>(6)?,
                ))
            },
        )
        .optional()
        .map_err(storage)?;
    let Some((stored, request, lease, physical, nonce, state, receipt)) = row else {
        return Ok(None);
    };
    let request: WorkspaceMergeOperation = serde_json::from_str(&request).map_err(storage)?;
    let lease: ExecutionLease = serde_json::from_str(&lease).map_err(storage)?;
    let state: WorkspaceMergeState =
        serde_json::from_value(serde_json::Value::String(state)).map_err(storage)?;
    let receipt: Option<WorkspaceMergeReceipt> = receipt
        .map(|raw| serde_json::from_str(&raw))
        .transpose()
        .map_err(storage)?;
    request.validate()?;
    if &lease.owner != owner
        || &request.id != id
        || digest(&lease, &request) != stored
        || physical != volume(owner, &request)
        || nonce.len() != 32
        || !nonce.bytes().all(|byte| byte.is_ascii_hexdigit())
        || (state == WorkspaceMergeState::Committed) != receipt.is_some()
    {
        return Err(ApplicationError::Conflict);
    }
    if let Some(receipt) = &receipt
        && (receipt.id != request.id
            || receipt.environment_id != request.environment_id
            || receipt.plan_digest != request.plan.digest()
            || receipt.state != WorkspaceMergeState::Committed
            || Some(receipt.revision) != request.expected_revision.checked_add(1))
    {
        return Err(ApplicationError::Conflict);
    }
    Ok(Some(MergeRecord {
        request,
        lease,
        volume: physical,
        nonce,
        state,
        receipt,
    }))
}
impl Ledger {
    pub(crate) fn scan_merges(&self, limit: u32) -> Result<Vec<MergeRecord>, ApplicationError> {
        if !(1..=32).contains(&limit) {
            return Err(ApplicationError::Invalid(
                "invalid merge delivery limit".to_owned(),
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
                    "SELECT tenant,principal,id FROM workspace_merge
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
                "SELECT COALESCE(MAX(scan_order),0) FROM workspace_merge",
                [],
                |row| row.get(0),
            )
            .map_err(storage)?;
        let mut result = Vec::new();
        for (tenant, principal, id) in rows {
            let owner = PrincipalKey {
                tenant_id: zuno_types::identity::TenantId::new(tenant).map_err(storage)?,
                principal_id: zuno_types::identity::PrincipalId::new(principal).map_err(storage)?,
            };
            let id = OperationId::new(id).map_err(storage)?;
            sequence = sequence.checked_add(1).ok_or(ApplicationError::Conflict)?;
            let record = read(&tx, &owner, &id)?.ok_or(ApplicationError::Conflict)?;
            tx.execute("UPDATE workspace_merge SET scan_order=?4 WHERE tenant=?1 AND principal=?2 AND id=?3",
                params![owner.tenant_id.as_str(),owner.principal_id.as_str(),id.as_str(),sequence]).map_err(storage)?;
            result.push(record);
        }
        tx.commit().map_err(storage)?;
        Ok(result)
    }
    pub(crate) fn defer_merge(
        &self,
        owner: &PrincipalKey,
        id: &OperationId,
    ) -> Result<(), ApplicationError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| ApplicationError::Unavailable)?;
        connection.execute("UPDATE workspace_merge SET retry_count=min(retry_count+1,5),
            retry_at=CAST(unixepoch('subsec')*1000 AS INTEGER)+min(1000*(1<<min(retry_count,5)),30000)
            WHERE tenant=?1 AND principal=?2 AND id=?3 AND acknowledged=0",
            params![owner.tenant_id.as_str(),owner.principal_id.as_str(),id.as_str()]).map_err(storage)?;
        Ok(())
    }
    pub(crate) fn merge_completion(
        &self,
        owner: &PrincipalKey,
        id: &OperationId,
    ) -> Result<Option<zuno_application::workspace_merge::WorkspaceMergeCompletion>, ApplicationError>
    {
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| ApplicationError::Unavailable)?;
        let tx = connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(storage)?;
        let record = read(&tx, owner, id)?.ok_or(ApplicationError::NotFound)?;
        let receipt = match record.state {
            WorkspaceMergeState::Committed => record.receipt.ok_or(ApplicationError::Conflict)?,
            WorkspaceMergeState::Cancelled => WorkspaceMergeReceipt {
                id: id.clone(),
                environment_id: record.request.environment_id.clone(),
                state: WorkspaceMergeState::Cancelled,
                plan_digest: record.request.plan.digest(),
                revision: record.request.expected_revision,
            },
            _ => return Ok(None),
        };
        let completion = zuno_application::workspace_merge::WorkspaceMergeCompletion {
            lease: record.lease,
            operation: record.request,
            receipt,
        };
        completion.validate()?;
        let encoded = serde_json::to_string(&completion).map_err(storage)?;
        let hash = zuno_orchestration::sha256_json(&serde_json::json!(completion));
        let previous:Option<String>=tx.query_row("SELECT completion_digest FROM workspace_merge WHERE tenant=?1 AND principal=?2 AND id=?3",
            params![owner.tenant_id.as_str(),owner.principal_id.as_str(),id.as_str()],|row|row.get(0)).map_err(storage)?;
        if previous.as_ref().is_some_and(|previous| previous != &hash) {
            return Err(ApplicationError::Conflict);
        }
        tx.execute("UPDATE workspace_merge SET completion=?4,completion_digest=?5 WHERE tenant=?1 AND principal=?2 AND id=?3 AND completion IS NULL",
            params![owner.tenant_id.as_str(),owner.principal_id.as_str(),id.as_str(),encoded,hash]).map_err(storage)?;
        tx.commit().map_err(storage)?;
        Ok(Some(completion))
    }
    pub(crate) fn acknowledge_merge(
        &self,
        completion: &zuno_application::workspace_merge::WorkspaceMergeCompletion,
    ) -> Result<(), ApplicationError> {
        completion.validate()?;
        let owner = &completion.lease.owner;
        let connection = self
            .connection
            .lock()
            .map_err(|_| ApplicationError::Unavailable)?;
        let changed=connection.execute("UPDATE workspace_merge SET acknowledged=1 WHERE tenant=?1 AND principal=?2 AND id=?3 AND completion_digest=?4",
            params![owner.tenant_id.as_str(),owner.principal_id.as_str(),completion.operation.id.as_str(),
                zuno_orchestration::sha256_json(&serde_json::json!(completion))]).map_err(storage)?;
        if changed != 1 {
            return Err(ApplicationError::Conflict);
        }
        Ok(())
    }

    /// A trusted cancellation may arrive before this gateway sees submit.
    /// Its tombstone does not seize or clear another operation's write slot.
    pub(crate) fn cancel_admitted_merge(
        &self,
        lease: &ExecutionLease,
        request: &WorkspaceMergeOperation,
    ) -> Result<MergeRecord, ApplicationError> {
        request.validate()?;
        let owner = &lease.owner;
        {
            let mut connection = self
                .connection
                .lock()
                .map_err(|_| ApplicationError::Unavailable)?;
            let tx = connection
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .map_err(storage)?;
            if let Some(existing) = read(&tx, owner, &request.id)? {
                if digest(&existing.lease, &existing.request) != digest(lease, request) {
                    return Err(ApplicationError::Conflict);
                }
            } else {
                let command:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM operation WHERE tenant=?1 AND principal=?2 AND id=?3)",
                    params![owner.tenant_id.as_str(),owner.principal_id.as_str(),request.id.as_str()],|row|row.get(0)).map_err(storage)?;
                if command {
                    return Err(ApplicationError::Conflict);
                }
                let environment = environment(&tx, owner, &request.environment_id)?;
                if environment.spec.session_id != lease.session_id {
                    return Err(ApplicationError::Conflict);
                }
                tx.execute("INSERT INTO workspace_merge(tenant,principal,id,environment_id,request_digest,request,lease,nonce,volume,state)
                    VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,'cancelled')",
                    params![owner.tenant_id.as_str(),owner.principal_id.as_str(),request.id.as_str(),request.environment_id.as_str(),
                        digest(lease,request),serde_json::to_string(request).map_err(storage)?,serde_json::to_string(lease).map_err(storage)?,
                        uuid::Uuid::new_v4().simple().to_string(),volume(owner,request)]).map_err(storage)?;
            }
            tx.commit().map_err(storage)?;
        }
        self.cancel_merge(owner, &request.id)
    }

    pub(crate) fn merge(
        &self,
        owner: &PrincipalKey,
        id: &OperationId,
    ) -> Result<MergeRecord, ApplicationError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| ApplicationError::Unavailable)?;
        read(&connection, owner, id)?.ok_or(ApplicationError::NotFound)
    }
    pub(crate) fn active_volume(
        &self,
        owner: &PrincipalKey,
        id: &EnvironmentId,
    ) -> Result<Option<MergeRecord>, ApplicationError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| ApplicationError::Unavailable)?;
        let selected: Option<String> = connection
            .query_row(
                "SELECT merge_id FROM environment_volume
            WHERE tenant=?1 AND principal=?2 AND environment_id=?3",
                params![
                    owner.tenant_id.as_str(),
                    owner.principal_id.as_str(),
                    id.as_str()
                ],
                |row| row.get(0),
            )
            .optional()
            .map_err(storage)?;
        selected
            .map(|value| {
                let value = read(
                    &connection,
                    owner,
                    &OperationId::new(value).map_err(storage)?,
                )?
                .ok_or(ApplicationError::Conflict)?;
                if value.state != WorkspaceMergeState::Committed
                    || &value.request.environment_id != id
                {
                    return Err(ApplicationError::Conflict);
                }
                Ok(value)
            })
            .transpose()
    }
    pub(crate) fn begin_merge(
        &self,
        lease: &ExecutionLease,
        request: &WorkspaceMergeOperation,
    ) -> Result<MergeRecord, ApplicationError> {
        request.validate()?;
        let owner = &lease.owner;
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| ApplicationError::Unavailable)?;
        let tx = connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(storage)?;
        if let Some(existing) = read(&tx, owner, &request.id)? {
            if digest(&existing.lease, &existing.request) != digest(lease, request) {
                return Err(ApplicationError::Conflict);
            }
            return Ok(existing);
        }
        let command: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM operation WHERE tenant=?1 AND principal=?2 AND id=?3)",
                params![
                    owner.tenant_id.as_str(),
                    owner.principal_id.as_str(),
                    request.id.as_str()
                ],
                |row| row.get(0),
            )
            .map_err(storage)?;
        if command {
            return Err(ApplicationError::Conflict);
        }
        let environment = environment(&tx, owner, &request.environment_id)?;
        if environment.spec.session_id != lease.session_id
            || environment.revision != request.expected_revision
        {
            return Err(ApplicationError::Conflict);
        }
        let changed=tx.execute("UPDATE environment SET active_operation=?4
            WHERE tenant=?1 AND principal=?2 AND id=?3 AND active_operation IS NULL AND revision=?5 AND state='active'",
            params![owner.tenant_id.as_str(),owner.principal_id.as_str(),request.environment_id.as_str(),request.id.as_str(),
                i64::try_from(request.expected_revision).map_err(storage)?]).map_err(storage)?;
        if changed != 1 {
            return Err(ApplicationError::Conflict);
        }
        tx.execute("INSERT INTO workspace_merge(tenant,principal,id,environment_id,request_digest,request,lease,nonce,volume,state)
            VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,'preparing')",
            params![owner.tenant_id.as_str(),owner.principal_id.as_str(),request.id.as_str(),request.environment_id.as_str(),digest(lease,request),
                serde_json::to_string(request).map_err(storage)?,serde_json::to_string(lease).map_err(storage)?,
                uuid::Uuid::new_v4().simple().to_string(),volume(owner,request)]).map_err(storage)?;
        let record = read(&tx, owner, &request.id)?.ok_or(ApplicationError::Conflict)?;
        tx.commit().map_err(storage)?;
        Ok(record)
    }
    pub(crate) fn publish_merge(
        &self,
        owner: &PrincipalKey,
        id: &OperationId,
    ) -> Result<WorkspaceMergeReceipt, ApplicationError> {
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
        if record.state != WorkspaceMergeState::Preparing {
            return Err(ApplicationError::Conflict);
        }
        let request = record.request;
        let revision = request
            .expected_revision
            .checked_add(1)
            .ok_or(ApplicationError::Conflict)?;
        let changed=tx.execute("UPDATE environment SET active_operation=NULL,revision=?5
            WHERE tenant=?1 AND principal=?2 AND id=?3 AND active_operation=?4 AND revision=?6 AND state='active'",
            params![owner.tenant_id.as_str(),owner.principal_id.as_str(),request.environment_id.as_str(),id.as_str(),
                i64::try_from(revision).map_err(storage)?,i64::try_from(request.expected_revision).map_err(storage)?]).map_err(storage)?;
        if changed != 1 {
            return Err(ApplicationError::Conflict);
        }
        let receipt = WorkspaceMergeReceipt {
            id: id.clone(),
            environment_id: request.environment_id.clone(),
            state: WorkspaceMergeState::Committed,
            plan_digest: request.plan.digest(),
            revision,
        };
        tx.execute("UPDATE workspace_merge SET state='committed',receipt=?4 WHERE tenant=?1 AND principal=?2 AND id=?3 AND state='preparing'",
            params![owner.tenant_id.as_str(),owner.principal_id.as_str(),id.as_str(),serde_json::to_string(&receipt).map_err(storage)?]).map_err(storage)?;
        tx.execute("INSERT INTO environment_volume(tenant,principal,environment_id,merge_id) VALUES(?1,?2,?3,?4)
            ON CONFLICT(tenant,principal,environment_id) DO UPDATE SET merge_id=excluded.merge_id",
            params![owner.tenant_id.as_str(),owner.principal_id.as_str(),request.environment_id.as_str(),id.as_str()]).map_err(storage)?;
        tx.commit().map_err(storage)?;
        Ok(receipt)
    }
    pub(crate) fn cancel_merge(
        &self,
        owner: &PrincipalKey,
        id: &OperationId,
    ) -> Result<MergeRecord, ApplicationError> {
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| ApplicationError::Unavailable)?;
        let tx = connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(storage)?;
        let record = read(&tx, owner, id)?.ok_or(ApplicationError::NotFound)?;
        if record.state != WorkspaceMergeState::Preparing {
            return Ok(record);
        }
        let changed=tx.execute("UPDATE environment SET active_operation=NULL WHERE tenant=?1 AND principal=?2 AND id=?3
            AND active_operation=?4 AND revision=?5",
            params![owner.tenant_id.as_str(),owner.principal_id.as_str(),record.request.environment_id.as_str(),id.as_str(),
                i64::try_from(record.request.expected_revision).map_err(storage)?]).map_err(storage)?;
        if changed != 1 {
            return Err(ApplicationError::Conflict);
        }
        tx.execute("UPDATE workspace_merge SET state='cancelled' WHERE tenant=?1 AND principal=?2 AND id=?3 AND state='preparing'",
            params![owner.tenant_id.as_str(),owner.principal_id.as_str(),id.as_str()]).map_err(storage)?;
        let record = read(&tx, owner, id)?.ok_or(ApplicationError::Conflict)?;
        tx.commit().map_err(storage)?;
        Ok(record)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request(command: &CommandOperation) -> WorkspaceMergeOperation {
        let snapshot = |id: &str, environment_id: EnvironmentId| EnvironmentSnapshot {
            id: EnvironmentSnapshotId::new(id).unwrap(),
            environment_id,
            revision: 1,
            sha256: "a".repeat(64),
            bytes: 1024,
        };
        WorkspaceMergeOperation {
            id: command.id.clone(),
            invocation_id: command.invocation_id.clone(),
            environment_id: command.environment_id.clone(),
            child_job_id: zuno_types::identity::JobId::new("child-job").unwrap(),
            expected_revision: 1,
            base: snapshot("base", command.environment_id.clone()),
            parent: snapshot("parent", command.environment_id.clone()),
            child: snapshot("child", EnvironmentId::new("child-env").unwrap()),
            plan: zuno_application::workspace_merge::plan(
                &Default::default(),
                &Default::default(),
                &Default::default(),
            )
            .unwrap(),
        }
    }
    #[test]
    fn merge_publication_is_atomic_replayable_and_exclusive_with_commands() {
        let (root, ledger, lease, command) = super::super::tests::fixture();
        let request = request(&command);
        let first = ledger.begin_merge(&lease, &request).unwrap();
        assert!(
            ledger
                .active_volume(&lease.owner, &command.environment_id)
                .unwrap()
                .is_none()
        );
        assert!(
            ledger
                .admit(&lease, &command, "forged-command".to_owned())
                .is_err()
        );
        assert!(
            ledger
                .require_idle(&lease.owner, &command.environment_id, 1)
                .is_err()
        );
        ledger.connection.lock().unwrap().execute_batch(
            "CREATE TRIGGER refuse_merge BEFORE INSERT ON environment_volume BEGIN SELECT RAISE(ABORT,'injected publication failure'); END;"
        ).unwrap();
        assert!(ledger.publish_merge(&lease.owner, &request.id).is_err());
        assert_eq!(
            ledger
                .environment(&lease.owner, &command.environment_id)
                .unwrap()
                .revision,
            1
        );
        assert_eq!(
            ledger.merge(&lease.owner, &request.id).unwrap().state,
            WorkspaceMergeState::Preparing
        );
        assert!(
            ledger
                .active_volume(&lease.owner, &command.environment_id)
                .unwrap()
                .is_none()
        );
        ledger
            .connection
            .lock()
            .unwrap()
            .execute_batch("DROP TRIGGER refuse_merge")
            .unwrap();
        drop(ledger);
        let ledger = Ledger::open(&root.path().join("gateway.sqlite")).unwrap();
        assert_eq!(
            ledger.begin_merge(&lease, &request).unwrap().nonce,
            first.nonce
        );
        let receipt = ledger.publish_merge(&lease.owner, &request.id).unwrap();
        assert_eq!(receipt.revision, 2);
        assert_eq!(
            ledger.publish_merge(&lease.owner, &request.id).unwrap(),
            receipt
        );
        assert_eq!(
            ledger
                .active_volume(&lease.owner, &command.environment_id)
                .unwrap()
                .unwrap()
                .volume,
            first.volume
        );
        assert_eq!(
            ledger
                .environment(&lease.owner, &command.environment_id)
                .unwrap()
                .revision,
            2
        );
        let mut changed = request;
        changed.child.sha256 = "b".repeat(64);
        assert!(ledger.begin_merge(&lease, &changed).is_err());
    }
    #[test]
    fn cancelled_merge_never_publishes_and_a_stale_revision_cannot_reserve() {
        let (_root, ledger, lease, command) = super::super::tests::fixture();
        let request = request(&command);
        ledger.begin_merge(&lease, &request).unwrap();
        ledger.cancel_merge(&lease.owner, &request.id).unwrap();
        assert!(ledger.publish_merge(&lease.owner, &request.id).is_err());
        assert!(
            ledger
                .active_volume(&lease.owner, &request.environment_id)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            ledger
                .require_idle(&lease.owner, &request.environment_id, 1)
                .unwrap()
                .revision,
            1
        );
        assert_eq!(
            ledger.begin_merge(&lease, &request).unwrap().state,
            WorkspaceMergeState::Cancelled
        );
        let mut stale = request;
        stale.id = OperationId::new("stale").unwrap();
        stale.expected_revision = 2;
        stale.parent.revision = 2;
        assert!(ledger.begin_merge(&lease, &stale).is_err());
    }
    #[test]
    fn a_cancellation_before_submit_cannot_clear_another_operation_or_be_started_later() {
        let (_root, ledger, lease, command) = super::super::tests::fixture();
        ledger
            .admit(&lease, &command, "command".to_owned())
            .unwrap();
        let mut request = request(&command);
        request.id = OperationId::new("never-start").unwrap();
        assert_eq!(
            ledger
                .cancel_admitted_merge(&lease, &request)
                .unwrap()
                .state,
            WorkspaceMergeState::Cancelled
        );
        assert!(
            ledger.begin_start(&lease.owner, &command.id).unwrap(),
            "cancellation must retain the other operation's slot"
        );
        assert_eq!(
            ledger.begin_merge(&lease, &request).unwrap().state,
            WorkspaceMergeState::Cancelled
        );
        assert!(ledger.publish_merge(&lease.owner, &request.id).is_err());
    }
    #[test]
    fn merge_delivery_survives_restart_and_release_requires_matching_acknowledgement() {
        let (root, ledger, lease, command) = super::super::tests::fixture();
        let request = request(&command);
        ledger.begin_merge(&lease, &request).unwrap();
        ledger.publish_merge(&lease.owner, &request.id).unwrap();
        let completion = ledger
            .merge_completion(&lease.owner, &request.id)
            .unwrap()
            .unwrap();
        assert!(
            ledger
                .release(&lease.owner, &request.environment_id, 2, false)
                .is_err()
        );
        ledger.defer_merge(&lease.owner, &request.id).unwrap();
        assert!(
            ledger.scan_merges(1).unwrap().is_empty(),
            "backoff must be positive"
        );
        drop(ledger);
        let ledger = Ledger::open(&root.path().join("gateway.sqlite")).unwrap();
        assert_eq!(
            ledger.merge_completion(&lease.owner, &request.id).unwrap(),
            Some(completion.clone())
        );
        let mut changed = completion.clone();
        changed.receipt.plan_digest = "0".repeat(64);
        assert!(ledger.acknowledge_merge(&changed).is_err());
        ledger.acknowledge_merge(&completion).unwrap();
        assert!(ledger.scan_merges(1).unwrap().is_empty());
        assert!(
            ledger
                .release(&lease.owner, &request.environment_id, 2, false)
                .unwrap()
                .is_some()
        );
    }
    #[test]
    fn format_three_upgrade_preserves_ledger_rows_and_rolls_back_before_marker() {
        let (root, current, lease, command) = super::super::tests::fixture();
        let path = root.path().join("format-three.sqlite");
        let old = Connection::open(&path).unwrap();
        old.execute_batch(include_str!("../fixtures/format2-base.sql"))
            .unwrap();
        old.execute_batch(include_str!("../fixtures/format2-delivery.sql"))
            .unwrap();
        old.execute_batch(include_str!("../fixtures/format3-fork.sql"))
            .unwrap();
        let environment = current
            .environment(&lease.owner, &command.environment_id)
            .unwrap();
        let spec = serde_json::to_string(&environment.spec).unwrap();
        old.execute(
            "INSERT INTO environment(tenant,principal,id,spec,revision) VALUES(?1,?2,?3,?4,1)",
            params![
                lease.owner.tenant_id.as_str(),
                lease.owner.principal_id.as_str(),
                environment.spec.id.as_str(),
                spec
            ],
        )
        .unwrap();
        old.execute_batch(
            "CREATE TRIGGER refuse_merge_marker BEFORE UPDATE OF version ON gateway_format
            BEGIN SELECT RAISE(ABORT,'injected migration failure'); END;",
        )
        .unwrap();
        old.execute(
            "INSERT INTO gateway_format VALUES(1,3,'enterprise-preview',?1,?2)",
            params![
                "6f30bab2746bb825d22ad6876644f1622d75a0fddd672d40ed8d5defd196b876",
                super::super::manifest(&old).unwrap()
            ],
        )
        .unwrap();
        drop(old);
        assert!(Ledger::open(&path).is_err());
        let old = Connection::open(&path).unwrap();
        assert_eq!(
            old.query_row("SELECT version FROM gateway_format", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            3
        );
        assert_eq!(
            old.query_row("SELECT spec FROM environment", [], |row| row
                .get::<_, String>(0))
                .unwrap(),
            spec
        );
        assert!(
            !old.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='workspace_merge')",
                [],
                |row| row.get::<_, bool>(0)
            )
            .unwrap()
        );
        old.execute_batch("DROP TRIGGER refuse_merge_marker")
            .unwrap();
        old.execute(
            "UPDATE gateway_format SET manifest=?1",
            params![super::super::manifest(&old).unwrap()],
        )
        .unwrap();
        drop(old);
        let migrated = Ledger::open(&path).unwrap();
        assert_eq!(
            migrated
                .environment(&lease.owner, &environment.spec.id)
                .unwrap(),
            environment
        );
        assert!(
            migrated
                .active_volume(&lease.owner, &environment.spec.id)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            migrated
                .connection
                .lock()
                .unwrap()
                .query_row("SELECT version FROM gateway_format", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            4
        );
    }
}
