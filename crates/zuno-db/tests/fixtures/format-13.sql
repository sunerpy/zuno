-- Preview database format 13 delta; no enterprise release was published for it.
-- Provenance: git show b56a2aeeeb32b3891ad0830ea4d129e8fb33432e:crates/zuno-db/src/schema/session_ownership.sql
-- Load after format-7.sql through format-12.sql.
-- Ownership is separate from user-editable session metadata and from the
-- application/policy identity captured by a particular request.
CREATE TABLE session_ownership (
  session_id text PRIMARY KEY NOT NULL,
  tenant_id text NOT NULL CHECK (length(tenant_id) BETWEEN 1 AND 128),
  principal_id text NOT NULL CHECK (length(principal_id) BETWEEN 1 AND 128),
  FOREIGN KEY (session_id) REFERENCES session(id) ON DELETE CASCADE
);

-- Published local databases have one explicit local owner. Import into an
-- enterprise namespace is a separate, authorized operation.
INSERT INTO session_ownership (session_id, tenant_id, principal_id)
  SELECT id, 'local', 'local-user' FROM session;

-- Raw local insertions keep the same invariant as the public creation service.
-- A service creating an authenticated root replaces this initial binding in
-- the same creation transaction, before any observer can see it.
CREATE TRIGGER session_ownership_insert AFTER INSERT ON session
BEGIN
  INSERT INTO session_ownership (session_id, tenant_id, principal_id)
  VALUES (
    NEW.id,
    COALESCE((SELECT tenant_id FROM session_ownership WHERE session_id=NEW.parent_id), 'local'),
    COALESCE((SELECT principal_id FROM session_ownership WHERE session_id=NEW.parent_id), 'local-user')
  );
END;

CREATE INDEX session_ownership_principal_idx
  ON session_ownership(tenant_id, principal_id, session_id);

-- A pre-runtime Job whose identity, result, cursor and status must survive
-- widening the table's subject constraint.
INSERT INTO agent_job(
  id,parent_session_id,logical_key,subject_kind,subject_payload,orchestration_snapshot,
  evidence_start_rowid,status,report_delivery,result,error,report_input_id,created_seq,
  settled_seq,time_created,time_updated,time_completed
) VALUES(
  'job_fixture_13','ses_fixture_0001','fixture-workflow','workflow',
  '{"kind":"workflow","runID":"run_fixture_13","workflow":"audit"}',NULL,
  23,'completed','quiet','{"answer":"保留已完成的工作"}',NULL,NULL,7,12,100,200,200
);
UPDATE zuno_schema SET format=13 WHERE singleton=1 AND format=12;
