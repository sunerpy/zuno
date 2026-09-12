-- A native Job belongs to its delegating parent, while its runtime execution
-- belongs to the distinct session driven by a Worker. Root Jobs keep both equal.
ALTER TABLE zuno_enterprise_preview.runtime_job
  DROP CONSTRAINT runtime_job_tenant_id_principal_id_job_id_session_id_fkey;
ALTER TABLE zuno_enterprise_preview.runtime_job
  ADD CONSTRAINT runtime_job_subject FOREIGN KEY(tenant_id,principal_id,job_id)
    REFERENCES zuno_enterprise_preview.agent_job(tenant_id,principal_id,id),
  ADD CONSTRAINT runtime_job_execution UNIQUE(tenant_id,principal_id,job_id,session_id);
-- PostgreSQL truncates generated constraint names. Match the exact historical
-- relation/column binding instead of guessing its truncated spelling.
DO $$
DECLARE old_binding text; matches integer;
BEGIN
  SELECT min(c.conname),count(*) INTO old_binding,matches
  FROM pg_constraint c
  WHERE c.contype='f'
    AND c.conrelid='zuno_enterprise_preview.runtime_session'::regclass
    AND c.confrelid='zuno_enterprise_preview.agent_job'::regclass
    AND (SELECT array_agg(a.attname::text ORDER BY k.ordinality)
      FROM unnest(c.conkey) WITH ORDINALITY k(attnum,ordinality)
      JOIN pg_attribute a ON a.attrelid=c.conrelid AND a.attnum=k.attnum)
      = ARRAY['tenant_id','principal_id','current_job_id','session_id'];
  IF matches<>1 THEN RAISE EXCEPTION 'historical runtime session binding is missing or ambiguous'; END IF;
  EXECUTE format('ALTER TABLE zuno_enterprise_preview.runtime_session DROP CONSTRAINT %I',old_binding);
END $$;
ALTER TABLE zuno_enterprise_preview.runtime_session
  ADD CONSTRAINT runtime_session_execution FOREIGN KEY(tenant_id,principal_id,current_job_id,session_id)
    REFERENCES zuno_enterprise_preview.runtime_job(tenant_id,principal_id,job_id,session_id);

CREATE TABLE zuno_enterprise_preview.runtime_child (
  tenant_id text NOT NULL,
  principal_id text NOT NULL,
  job_id text NOT NULL,
  parent_job_id text NOT NULL,
  parent_session_id text NOT NULL,
  child_session_id text NOT NULL,
  invocation_id text NOT NULL,
  logical_key text NOT NULL,
  request_digest text NOT NULL CHECK(length(request_digest)=64),
  invocation jsonb NOT NULL CHECK(jsonb_typeof(invocation)='object'),
  definition jsonb NOT NULL CHECK(jsonb_typeof(definition)='object'),
  selection jsonb NOT NULL CHECK(jsonb_typeof(selection)='object'),
  reference jsonb NOT NULL CHECK(jsonb_typeof(reference)='object'),
  delivery text NOT NULL CHECK(delivery IN('foreground','next_step','quiet')),
  state text NOT NULL CHECK(state IN('staged','active','completed','consumed','cancelled','uncertain')),
  activated_job_id text,
  completion jsonb CHECK(completion IS NULL OR jsonb_typeof(completion)='object'),
  completion_digest text CHECK(completion_digest IS NULL OR length(completion_digest)=64),
  notification_pending boolean NOT NULL DEFAULT false,
  time_created bigint NOT NULL,
  time_updated bigint NOT NULL,
  PRIMARY KEY(tenant_id,principal_id,job_id),
  UNIQUE(tenant_id,principal_id,parent_job_id,invocation_id),
  FOREIGN KEY(tenant_id,principal_id,parent_job_id,parent_session_id)
    REFERENCES zuno_enterprise_preview.runtime_job(tenant_id,principal_id,job_id,session_id),
  FOREIGN KEY(tenant_id,principal_id,activated_job_id,child_session_id)
    REFERENCES zuno_enterprise_preview.runtime_job(tenant_id,principal_id,job_id,session_id),
  CHECK(activated_job_id IS NULL OR activated_job_id=job_id),
  CHECK((state IN('active','completed','consumed','uncertain'))=(activated_job_id IS NOT NULL)),
  CHECK((completion IS NULL)=(completion_digest IS NULL)),
  CHECK(NOT notification_pending OR completion IS NOT NULL)
);
CREATE UNIQUE INDEX runtime_child_unsettled_logical
  ON zuno_enterprise_preview.runtime_child(tenant_id,principal_id,parent_session_id,logical_key)
  WHERE state IN('staged','active','uncertain');
CREATE UNIQUE INDEX runtime_child_session_reservation
  ON zuno_enterprise_preview.runtime_child(tenant_id,principal_id,child_session_id)
  WHERE state IN('staged','active','uncertain');
CREATE INDEX runtime_child_delivery
  ON zuno_enterprise_preview.runtime_child(tenant_id,principal_id,parent_session_id,job_id)
  WHERE notification_pending;
