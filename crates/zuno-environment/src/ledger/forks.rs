use super::*;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ForkRecord {
    snapshot: EnvironmentSnapshot,
    target: EnvironmentSpec,
    /// An instance-specific ownership marker. A new ledger must not adopt and
    /// erase a similarly named volume left by a different ledger instance.
    nonce: String,
}
pub(crate) enum ForkPreparation {
    Restore,
    Committed(Environment),
}

fn read(
    connection: &Connection,
    owner: &PrincipalKey,
    id: &EnvironmentId,
) -> Result<Option<(ForkRecord, String)>, ApplicationError> {
    let row:Option<(String,String,String)>=connection.query_row(
        "SELECT data,request_digest,state FROM environment_fork WHERE tenant=?1 AND principal=?2 AND environment_id=?3",
        params![owner.tenant_id.as_str(),owner.principal_id.as_str(),id.as_str()],
        |row|Ok((row.get(0)?,row.get(1)?,row.get(2)?)),
    ).optional().map_err(storage)?;
    row.map(|(data, digest, state)| {
        let record: ForkRecord = serde_json::from_str(&data).map_err(storage)?;
        record.target.validate()?;
        if record.target.id != *id
            || uuid::Uuid::parse_str(&record.nonce).is_err()
            || digest
                != zuno_orchestration::sha256_json(&serde_json::json!([
                    owner,
                    record.snapshot,
                    record.target
                ]))
        {
            return Err(ApplicationError::Conflict);
        }
        Ok((record, state))
    })
    .transpose()
}

impl Ledger {
    pub(crate) fn require_unreserved_environment(
        &self,
        owner: &PrincipalKey,
        id: &EnvironmentId,
    ) -> Result<(), ApplicationError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| ApplicationError::Unavailable)?;
        if read(&connection, owner, id)?.is_some() {
            return Err(ApplicationError::Conflict);
        }
        Ok(())
    }

    pub(crate) fn fork_nonce(
        &self,
        owner: &PrincipalKey,
        id: &EnvironmentId,
    ) -> Result<Option<String>, ApplicationError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| ApplicationError::Unavailable)?;
        Ok(read(&connection, owner, id)?.map(|(record, _)| record.nonce))
    }

    pub(crate) fn begin_fork(
        &self,
        owner: &PrincipalKey,
        snapshot: &EnvironmentSnapshot,
        target: &EnvironmentSpec,
    ) -> Result<ForkPreparation, ApplicationError> {
        target.validate()?;
        if snapshot.environment_id == target.id {
            return Err(ApplicationError::Conflict);
        }
        if self.snapshot(owner, &snapshot.id)? != *snapshot {
            return Err(ApplicationError::Conflict);
        }
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| ApplicationError::Unavailable)?;
        let tx = connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(storage)?;
        if let Some((record, state)) = read(&tx, owner, &target.id)? {
            if record.snapshot != *snapshot || record.target != *target {
                return Err(ApplicationError::Conflict);
            }
            return if state == "committed" {
                Ok(ForkPreparation::Committed(environment(
                    &tx, owner, &target.id,
                )?))
            } else {
                Ok(ForkPreparation::Restore)
            };
        }
        let exists:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM environment WHERE tenant=?1 AND principal=?2 AND id=?3)",
            params![owner.tenant_id.as_str(),owner.principal_id.as_str(),target.id.as_str()],|row|row.get(0)).map_err(storage)?;
        if exists {
            return Err(ApplicationError::Conflict);
        }
        let record = ForkRecord {
            snapshot: snapshot.clone(),
            target: target.clone(),
            nonce: uuid::Uuid::now_v7().simple().to_string(),
        };
        tx.execute("INSERT INTO environment_fork(tenant,principal,environment_id,snapshot_id,request_digest,data,state)
            VALUES(?1,?2,?3,?4,?5,?6,'restoring')",
            params![owner.tenant_id.as_str(),owner.principal_id.as_str(),target.id.as_str(),snapshot.id.as_str(),
                zuno_orchestration::sha256_json(&serde_json::json!([owner,snapshot,target])),serde_json::to_string(&record).map_err(storage)?],
        ).map_err(storage)?;
        tx.commit().map_err(storage)?;
        Ok(ForkPreparation::Restore)
    }

    /// Publication is atomic. Ordinary acquire cannot publish a reserved target.
    pub(crate) fn finish_fork(
        &self,
        owner: &PrincipalKey,
        target: &EnvironmentSpec,
    ) -> Result<Environment, ApplicationError> {
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| ApplicationError::Unavailable)?;
        let tx = connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(storage)?;
        let (record, state) = read(&tx, owner, &target.id)?.ok_or(ApplicationError::NotFound)?;
        if record.target != *target {
            return Err(ApplicationError::Conflict);
        }
        if state == "committed" {
            return environment(&tx, owner, &target.id);
        }
        tx.execute(
            "INSERT INTO environment(tenant,principal,id,spec,revision) VALUES(?1,?2,?3,?4,1)",
            params![
                owner.tenant_id.as_str(),
                owner.principal_id.as_str(),
                target.id.as_str(),
                serde_json::to_string(target).map_err(storage)?
            ],
        )
        .map_err(storage)?;
        let changed=tx.execute("UPDATE environment_fork SET state='committed' WHERE tenant=?1 AND principal=?2 AND environment_id=?3 AND state='restoring'",
            params![owner.tenant_id.as_str(),owner.principal_id.as_str(),target.id.as_str()]).map_err(storage)?;
        if changed != 1 {
            return Err(ApplicationError::Conflict);
        }
        let result = environment(&tx, owner, &target.id)?;
        tx.commit().map_err(storage)?;
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zuno_types::identity::{PrincipalScope, SessionId};

    fn fixture() -> (
        tempfile::TempDir,
        Ledger,
        PrincipalKey,
        EnvironmentSnapshot,
        EnvironmentSpec,
    ) {
        let root = tempfile::tempdir().unwrap();
        let ledger = Ledger::open(&root.path().join("gateway.sqlite")).unwrap();
        let owner = PrincipalScope::local().owner();
        let source = EnvironmentSpec {
            id: EnvironmentId::new("source").unwrap(),
            session_id: SessionId::new("parent").unwrap(),
            image: format!("image@sha256:{}", "a".repeat(64)),
            memory_bytes: 67108864,
            pids_limit: 32,
            cpu_millis: 500,
        };
        ledger.create_environment(&owner, &source).unwrap();
        let snapshot = EnvironmentSnapshot {
            id: EnvironmentSnapshotId::new("snapshot").unwrap(),
            environment_id: source.id.clone(),
            revision: 1,
            sha256: "b".repeat(64),
            bytes: 1024,
        };
        ledger.put_snapshot(&owner, &snapshot).unwrap();
        let target = EnvironmentSpec {
            id: EnvironmentId::new("child").unwrap(),
            session_id: SessionId::new("child").unwrap(),
            ..source
        };
        (root, ledger, owner, snapshot, target)
    }

    #[test]
    fn fork_reservation_cannot_be_acquired_or_rebound_and_survives_restart() {
        let (root, ledger, owner, snapshot, target) = fixture();
        assert!(matches!(
            ledger.begin_fork(&owner, &snapshot, &target).unwrap(),
            ForkPreparation::Restore
        ));
        let nonce = ledger.fork_nonce(&owner, &target.id).unwrap().unwrap();
        assert!(ledger.create_environment(&owner, &target).is_err());
        assert!(matches!(
            ledger.environment(&owner, &target.id),
            Err(ApplicationError::NotFound)
        ));
        let mut changed = target.clone();
        changed.memory_bytes *= 2;
        assert!(ledger.begin_fork(&owner, &snapshot, &changed).is_err());
        drop(ledger);
        let ledger = Ledger::open(&root.path().join("gateway.sqlite")).unwrap();
        assert_eq!(
            ledger.fork_nonce(&owner, &target.id).unwrap().as_deref(),
            Some(nonce.as_str())
        );
        let published = ledger.finish_fork(&owner, &target).unwrap();
        let ForkPreparation::Committed(replayed) =
            ledger.begin_fork(&owner, &snapshot, &target).unwrap()
        else {
            panic!("fork publication");
        };
        assert_eq!(published, replayed);
    }

    #[test]
    fn target_publication_and_fork_receipt_roll_back_together() {
        let (_root, ledger, owner, snapshot, target) = fixture();
        ledger.begin_fork(&owner, &snapshot, &target).unwrap();
        ledger.connection.lock().unwrap().execute_batch(
            "CREATE TRIGGER refuse_fork BEFORE UPDATE OF state ON environment_fork
             WHEN NEW.state='committed' BEGIN SELECT RAISE(ABORT,'injected publication failure'); END;"
        ).unwrap();
        assert!(ledger.finish_fork(&owner, &target).is_err());
        assert!(!ledger.contains_environment(&owner, &target.id).unwrap());
        assert!(matches!(
            ledger.begin_fork(&owner, &snapshot, &target).unwrap(),
            ForkPreparation::Restore
        ));
        ledger
            .connection
            .lock()
            .unwrap()
            .execute_batch("DROP TRIGGER refuse_fork")
            .unwrap();
        ledger.finish_fork(&owner, &target).unwrap();
    }

    #[test]
    fn exact_format_two_rows_survive_atomic_fork_migration() {
        let (root, source, owner, snapshot, target) = fixture();
        let path = root.path().join("format-two.sqlite");
        let old = Connection::open(&path).unwrap();
        old.execute_batch(include_str!("../fixtures/format2-base.sql"))
            .unwrap();
        old.execute_batch(include_str!("../fixtures/format2-delivery.sql"))
            .unwrap();
        let environment = source
            .environment(&owner, &snapshot.environment_id)
            .unwrap();
        let spec = serde_json::to_string(&environment.spec).unwrap();
        let snap = serde_json::to_string(&snapshot).unwrap();
        old.execute(
            "INSERT INTO environment(tenant,principal,id,spec,revision) VALUES(?1,?2,?3,?4,1)",
            params![
                owner.tenant_id.as_str(),
                owner.principal_id.as_str(),
                environment.spec.id.as_str(),
                spec
            ],
        )
        .unwrap();
        old.execute(
            "INSERT INTO snapshot(tenant,principal,id,data) VALUES(?1,?2,?3,?4)",
            params![
                owner.tenant_id.as_str(),
                owner.principal_id.as_str(),
                snapshot.id.as_str(),
                snap
            ],
        )
        .unwrap();
        old.execute_batch(
            "CREATE TRIGGER refuse_fork_marker BEFORE UPDATE OF version ON gateway_format
            BEGIN SELECT RAISE(ABORT,'injected migration marker failure'); END;",
        )
        .unwrap();
        old.execute(
            "INSERT INTO gateway_format VALUES(1,2,'enterprise-preview',?1,?2)",
            params![
                "165b21d316b41fbd91567f8331c96d80c7abc831ab89904d771d7bfd56edd68d",
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
            2
        );
        assert!(
            !old.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='environment_fork')",
                [],
                |row| row.get::<_, bool>(0)
            )
            .unwrap()
        );
        assert_eq!(
            old.query_row("SELECT spec FROM environment", [], |row| row
                .get::<_, String>(0))
                .unwrap(),
            spec
        );
        assert_eq!(
            old.query_row("SELECT data FROM snapshot", [], |row| row
                .get::<_, String>(0))
                .unwrap(),
            snap
        );
        old.execute_batch("DROP TRIGGER refuse_fork_marker")
            .unwrap();
        old.execute(
            "UPDATE gateway_format SET manifest=?1",
            [super::super::manifest(&old).unwrap()],
        )
        .unwrap();
        drop(old);
        let migrated = Ledger::open(&path).unwrap();
        assert_eq!(
            migrated.environment(&owner, &environment.spec.id).unwrap(),
            environment
        );
        assert_eq!(migrated.snapshot(&owner, &snapshot.id).unwrap(), snapshot);
        migrated.begin_fork(&owner, &snapshot, &target).unwrap();
        migrated.finish_fork(&owner, &target).unwrap();
        assert_eq!(
            migrated
                .connection
                .lock()
                .unwrap()
                .query_row("SELECT version FROM gateway_format", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            super::super::FORMAT
        );
    }
}
