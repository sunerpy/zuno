-- Native Job identities retain root/child/product/workflow subject semantics.
CREATE TABLE zuno_enterprise_preview.agent_job (
  tenant_id text NOT NULL,
  principal_id text NOT NULL,
  id text NOT NULL,
  parent_session_id text NOT NULL,
  subject_kind text NOT NULL CHECK(subject_kind IN('root-turn','child-session','product-agent','workflow')),
  subject_payload jsonb NOT NULL CHECK(jsonb_typeof(subject_payload)='object'),
  status text NOT NULL CHECK(status IN('queued','running','completed','failed','cancelled','uncertain')),
  report_delivery text NOT NULL CHECK(report_delivery IN('next-step','quiet')),
  result jsonb,
  error text,
  created_seq bigint NOT NULL CHECK(created_seq>=0),
  settled_seq bigint CHECK(settled_seq>=0),
  time_created bigint NOT NULL,
  time_updated bigint NOT NULL,
  time_completed bigint,
  PRIMARY KEY(tenant_id,principal_id,id),
  UNIQUE(tenant_id,principal_id,id,parent_session_id),
  FOREIGN KEY(tenant_id,principal_id,parent_session_id)
    REFERENCES zuno_enterprise_preview.session(tenant_id,principal_id,id),
  CHECK(subject_kind<>'root-turn' OR report_delivery='quiet')
);
CREATE TABLE zuno_enterprise_preview.runtime_session (
  tenant_id text NOT NULL,
  principal_id text NOT NULL,
  session_id text NOT NULL,
  input_version bigint NOT NULL DEFAULT 0 CHECK(input_version>=0),
  current_job_id text,
  lease_epoch bigint NOT NULL DEFAULT 0 CHECK(lease_epoch>=0),
  lease_job_id text,
  lease_attempt_id text,
  lease_worker_id text,
  lease_expires bigint,
  PRIMARY KEY(tenant_id,principal_id,session_id),
  FOREIGN KEY(tenant_id,principal_id,session_id)
    REFERENCES zuno_enterprise_preview.session(tenant_id,principal_id,id),
  FOREIGN KEY(tenant_id,principal_id,current_job_id,session_id)
    REFERENCES zuno_enterprise_preview.agent_job(tenant_id,principal_id,id,parent_session_id),
  CHECK((lease_job_id IS NULL AND lease_attempt_id IS NULL AND lease_worker_id IS NULL AND lease_expires IS NULL)
     OR (lease_job_id IS NOT NULL AND current_job_id IS NOT NULL AND lease_job_id=current_job_id AND lease_attempt_id IS NOT NULL
       AND lease_worker_id IS NOT NULL AND lease_expires IS NOT NULL))
);
ALTER TABLE zuno_enterprise_preview.input
  ADD CONSTRAINT input_session_binding UNIQUE(tenant_id,principal_id,session_id,id);
CREATE TABLE zuno_enterprise_preview.runtime_job (
  tenant_id text NOT NULL,
  principal_id text NOT NULL,
  job_id text NOT NULL,
  session_id text NOT NULL,
  turn_id text NOT NULL,
  input_id text NOT NULL,
  request_digest text NOT NULL CHECK(length(request_digest)=64),
  principal jsonb NOT NULL CHECK(jsonb_typeof(principal)='object'),
  configuration jsonb NOT NULL CHECK(jsonb_typeof(configuration)='object'),
  phase text NOT NULL CHECK(phase IN('ready','running','paused','completed','failed','cancelled','uncertain')),
  checkpoint jsonb CHECK(checkpoint IS NULL OR jsonb_typeof(checkpoint)='object'),
  checkpoint_version bigint NOT NULL DEFAULT 0 CHECK(checkpoint_version>=0),
  input_version bigint NOT NULL CHECK(input_version>=0),
  active_attempt_id text,
  ready_at bigint NOT NULL,
  time_created bigint NOT NULL,
  time_updated bigint NOT NULL,
  PRIMARY KEY(tenant_id,principal_id,job_id),
  UNIQUE(tenant_id,principal_id,session_id,turn_id),
  FOREIGN KEY(tenant_id,principal_id,job_id,session_id)
    REFERENCES zuno_enterprise_preview.agent_job(tenant_id,principal_id,id,parent_session_id),
  FOREIGN KEY(tenant_id,principal_id,session_id,input_id)
    REFERENCES zuno_enterprise_preview.input(tenant_id,principal_id,session_id,id),
  CHECK((phase='running')=(active_attempt_id IS NOT NULL)),
  CHECK((checkpoint_version=0)=(checkpoint IS NULL))
);
CREATE INDEX runtime_job_ready_idx
  ON zuno_enterprise_preview.runtime_job(tenant_id,principal_id,ready_at,job_id) WHERE phase='ready';
CREATE TABLE zuno_enterprise_preview.runtime_attempt (
  tenant_id text NOT NULL,
  principal_id text NOT NULL,
  id text NOT NULL,
  job_id text NOT NULL,
  worker_id text NOT NULL,
  lease_epoch bigint NOT NULL CHECK(lease_epoch>0),
  state text NOT NULL CHECK(state IN('running','released','completed','lost')),
  started_at bigint NOT NULL,
  finished_at bigint,
  PRIMARY KEY(tenant_id,principal_id,id),
  UNIQUE(tenant_id,principal_id,id,job_id),
  FOREIGN KEY(tenant_id,principal_id,job_id)
    REFERENCES zuno_enterprise_preview.runtime_job(tenant_id,principal_id,job_id)
);
ALTER TABLE zuno_enterprise_preview.runtime_session
  ADD CONSTRAINT runtime_session_attempt FOREIGN KEY(tenant_id,principal_id,lease_attempt_id,lease_job_id)
    REFERENCES zuno_enterprise_preview.runtime_attempt(tenant_id,principal_id,id,job_id)
    DEFERRABLE INITIALLY DEFERRED;
ALTER TABLE zuno_enterprise_preview.runtime_job
  ADD CONSTRAINT runtime_job_attempt FOREIGN KEY(tenant_id,principal_id,active_attempt_id,job_id)
    REFERENCES zuno_enterprise_preview.runtime_attempt(tenant_id,principal_id,id,job_id)
    DEFERRABLE INITIALLY DEFERRED;
CREATE TABLE zuno_enterprise_preview.runtime_owner_schedule (
  tenant_id text NOT NULL,
  principal_id text NOT NULL,
  last_dispatch_sequence bigint NOT NULL DEFAULT 0 CHECK(last_dispatch_sequence>=0),
  PRIMARY KEY(tenant_id,principal_id)
);

-- Version 1 only admitted inputs; each preserved row contributes one revision.
-- Exclusive DDL locks and this transaction prevent concurrent readers observing
-- the temporary owner-only migration access. FORCE is restored before commit.
ALTER TABLE zuno_enterprise_preview.session NO FORCE ROW LEVEL SECURITY;
ALTER TABLE zuno_enterprise_preview.input NO FORCE ROW LEVEL SECURITY;
INSERT INTO zuno_enterprise_preview.runtime_session(tenant_id,principal_id,session_id,input_version)
SELECT s.tenant_id,s.principal_id,s.id,count(i.id)
FROM zuno_enterprise_preview.session s LEFT JOIN zuno_enterprise_preview.input i
  ON i.tenant_id=s.tenant_id AND i.principal_id=s.principal_id AND i.session_id=s.id
GROUP BY s.tenant_id,s.principal_id,s.id;
ALTER TABLE zuno_enterprise_preview.session FORCE ROW LEVEL SECURITY;
ALTER TABLE zuno_enterprise_preview.input FORCE ROW LEVEL SECURITY;

CREATE FUNCTION zuno_enterprise_preview.create_runtime_session() RETURNS trigger
LANGUAGE plpgsql SET search_path=pg_catalog AS $$
BEGIN
  INSERT INTO zuno_enterprise_preview.runtime_session(tenant_id,principal_id,session_id)
    VALUES(NEW.tenant_id,NEW.principal_id,NEW.id);
  RETURN NEW;
END $$;
CREATE TRIGGER create_runtime_session AFTER INSERT ON zuno_enterprise_preview.session
FOR EACH ROW EXECUTE FUNCTION zuno_enterprise_preview.create_runtime_session();
CREATE FUNCTION zuno_enterprise_preview.advance_input_version() RETURNS trigger
LANGUAGE plpgsql SET search_path=pg_catalog AS $$
BEGIN
  UPDATE zuno_enterprise_preview.runtime_session SET input_version=input_version+1
    WHERE tenant_id=NEW.tenant_id AND principal_id=NEW.principal_id AND session_id=NEW.session_id;
  RETURN NEW;
END $$;
CREATE TRIGGER runtime_input_insert AFTER INSERT ON zuno_enterprise_preview.input
FOR EACH ROW EXECUTE FUNCTION zuno_enterprise_preview.advance_input_version();
CREATE TRIGGER runtime_input_edit AFTER UPDATE ON zuno_enterprise_preview.input
FOR EACH ROW WHEN(OLD.prompt IS DISTINCT FROM NEW.prompt OR OLD.revision<>NEW.revision
  OR (NEW.state='cancelled' AND OLD.state<>'cancelled'))
EXECUTE FUNCTION zuno_enterprise_preview.advance_input_version();

-- Only the migration owner can see the complete scheduling catalog. The
-- security-definer function exposes bounded owner/order metadata, never Jobs,
-- sessions, prompts, checkpoints, credentials, or arbitrary SQL.
CREATE POLICY dispatch_catalog_owner ON zuno_enterprise_preview.runtime_owner_schedule
FOR SELECT USING(current_user=pg_get_userbyid(
  (SELECT nspowner FROM pg_namespace WHERE nspname='zuno_enterprise_preview')));
CREATE FUNCTION zuno_enterprise_preview.dispatch_owners(requested_tenant text)
RETURNS TABLE(principal_id text,last_dispatch_sequence bigint,dispatch_clock bigint)
LANGUAGE sql STABLE SECURITY DEFINER SET search_path=pg_catalog SET row_security=on AS $$
  SELECT q.principal_id,q.last_dispatch_sequence,max(q.last_dispatch_sequence) OVER()
  FROM zuno_enterprise_preview.runtime_owner_schedule q
  WHERE q.tenant_id=requested_tenant
  ORDER BY q.last_dispatch_sequence,q.principal_id LIMIT 64
$$;
REVOKE ALL ON FUNCTION zuno_enterprise_preview.dispatch_owners(text) FROM PUBLIC;
REVOKE ALL ON FUNCTION zuno_enterprise_preview.create_runtime_session() FROM PUBLIC;
REVOKE ALL ON FUNCTION zuno_enterprise_preview.advance_input_version() FROM PUBLIC;
